//! Kiln credential vault (PRD §5.5, §7.5).
//!
//! * Each secret is encrypted with **ChaCha20-Poly1305** under the master key
//!   with a random 96-bit nonce. The AEAD associated data is
//!   `user_id || credential_id`, so a ciphertext can't be moved to another
//!   user or credential row.
//! * The master key lives in the OS keychain, or `<data_dir>/master.key`
//!   (0600) when no keychain is available ([`MasterKey::load_or_create`]).
//! * Secrets are decrypted only at dispatch ([`Vault::dispatch_env`]), which
//!   also returns a [`Redactor`] for scrubbing the run's output.
//! * Secrets are never returned by listing APIs and never logged.

mod key;
mod redact;
mod tester;

pub use key::{
    ENV_MASTER_KEY, ENV_NO_KEYCHAIN, KEY_FILE, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE, KeyOptions,
    KeySource, MasterKey, NONCE_LEN,
};
pub use redact::{MIN_SECRET_LEN, REDACTED, Redactor, mask, scan_for_secrets};
pub use tester::{CredentialTester, HttpTester, TestOutcome, litellm_models_url};

use kiln_store::{NewCredential, Store, StoreError};
use kiln_types::api::{CreateCredentialRequest, CredentialTestResponse, RotateCredentialRequest};
use kiln_types::{CredentialHealth, CredentialId, CredentialKind, CredentialSummary, UserId};
use std::sync::Arc;
use zeroize::Zeroizing;

/// Env var names for each credential kind.
pub mod env {
    /// Claude subscription OAuth token (Claude Agent SDK).
    pub const CLAUDE_CODE_OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";
    /// Anthropic API key.
    pub const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
    /// OpenAI API key (Codex).
    pub const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
    /// OpenAI-compatible base URL (Codex via LiteLLM).
    pub const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
    /// LiteLLM proxy base URL.
    pub const LITELLM_BASE_URL: &str = "LITELLM_BASE_URL";
    /// LiteLLM proxy key.
    pub const LITELLM_API_KEY: &str = "LITELLM_API_KEY";
}

/// Settings key holding a value sealed under the master key, used to detect
/// a changed/lost master key before secrets silently fail to decrypt.
pub const KEY_CHECK_SETTING: &str = "vault_key_check";
const KEY_CHECK_PLAINTEXT: &[u8] = b"kiln-vault-key-check-v1";

/// Vault errors.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// Encryption or decryption failed (wrong key, tampered data, wrong owner).
    #[error("credential could not be decrypted")]
    Crypto,
    /// The master key material is malformed.
    #[error("invalid master key: {0}")]
    BadKey(String),
    /// The master key does not match the one the vault was created with.
    #[error(
        "the vault master key does not match this database (was the keychain entry or master.key replaced?)"
    )]
    KeyMismatch,
    /// Request validation failed.
    #[error("{0}")]
    Invalid(String),
    /// The credential does not exist or belongs to someone else.
    #[error("credential not found")]
    NotFound,
    /// Store failure.
    #[error(transparent)]
    Store(StoreError),
    /// File-system error while loading the key.
    #[error("key file error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<StoreError> for VaultError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => VaultError::NotFound,
            StoreError::Invalid(m) => VaultError::Invalid(m),
            other => VaultError::Store(other),
        }
    }
}

/// Result alias for vault operations.
pub type Result<T, E = VaultError> = std::result::Result<T, E>;

/// Decrypted secrets for one dispatch: env vars to inject into the run's
/// container plus a redactor primed with every raw value.
#[derive(Clone, Default)]
pub struct DispatchSecrets {
    /// `(NAME, value)` pairs, in a stable order.
    pub env: Vec<(String, String)>,
    /// Redacts every injected secret value.
    pub redactor: Redactor,
}

impl std::fmt::Debug for DispatchSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("DispatchSecrets")
            .field("env", &names)
            .finish_non_exhaustive()
    }
}

impl DispatchSecrets {
    /// The value injected for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn set_if_absent(&mut self, name: &str, value: &str) {
        if self.get(name).is_none() {
            self.env.push((name.to_string(), value.to_string()));
        }
    }
}

/// Env var names a custom (`Other`) credential may not override.
fn reserved_env(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH", "HOME", "USER", "SHELL", "PWD", "TMPDIR", "HOSTNAME", "TERM", "LANG",
    ];
    const PREFIXES: &[&str] = &["KILN_", "LD_", "DYLD_"];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 128
}

fn validate_base_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let ok = (u.starts_with("http://") || u.starts_with("https://"))
        && u.len() > "https://".len()
        && !u.chars().any(|c| c.is_whitespace() || c.is_control());
    if !ok {
        return Err(VaultError::Invalid(
            "base URL must be an http(s) URL".into(),
        ));
    }
    Ok(u.to_string())
}

fn validate_secret(secret: &str) -> Result<&str> {
    let s = secret.trim();
    if s.is_empty() {
        return Err(VaultError::Invalid("secret is empty".into()));
    }
    if s.len() > 16 * 1024 {
        return Err(VaultError::Invalid("secret is too long".into()));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(VaultError::Invalid(
            "secret contains control characters".into(),
        ));
    }
    Ok(s)
}

/// One decrypted credential during dispatch.
struct Decrypted {
    kind: CredentialKind,
    secret: Zeroizing<String>,
    base_url: Option<String>,
    env_var: Option<String>,
}

/// The per-user credential vault.
#[derive(Clone)]
pub struct Vault {
    key: MasterKey,
    store: Store,
    tester: Arc<dyn CredentialTester>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").finish_non_exhaustive()
    }
}

impl Vault {
    /// A vault using live HTTP credential tests.
    pub fn new(key: MasterKey, store: Store) -> Self {
        Self::with_tester(key, store, Arc::new(HttpTester::new()))
    }

    /// A vault with a custom credential tester (tests use a fake).
    pub fn with_tester(key: MasterKey, store: Store, tester: Arc<dyn CredentialTester>) -> Self {
        Self { key, store, tester }
    }

    /// Verify that `key` is the master key this database was set up with
    /// (recording it on first use). Call once at startup: a mismatch means
    /// every stored secret would fail to decrypt.
    pub async fn verify_key(&self) -> Result<()> {
        let check_ids = (UserId::nil(), CredentialId::nil());
        match self
            .store
            .get_setting::<(Vec<u8>, Vec<u8>)>(KEY_CHECK_SETTING)
            .await?
        {
            Some((ct, nonce)) => {
                let pt = self
                    .key
                    .open(check_ids.0, check_ids.1, &ct, &nonce)
                    .map_err(|_| VaultError::KeyMismatch)?;
                if pt.as_slice() != KEY_CHECK_PLAINTEXT {
                    return Err(VaultError::KeyMismatch);
                }
                Ok(())
            }
            None => {
                let (ct, nonce) = self
                    .key
                    .seal(check_ids.0, check_ids.1, KEY_CHECK_PLAINTEXT)?;
                self.store
                    .set_setting(KEY_CHECK_SETTING, &(ct, nonce.to_vec()))
                    .await?;
                Ok(())
            }
        }
    }

    /// Encrypt and store a new credential. The secret is never shown again.
    pub async fn add(
        &self,
        user: UserId,
        req: CreateCredentialRequest,
    ) -> Result<CredentialSummary> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(VaultError::Invalid("credential name is required".into()));
        }
        let secret = validate_secret(&req.secret)?;
        let base_url = match (
            req.kind,
            req.base_url.as_deref().filter(|s| !s.trim().is_empty()),
        ) {
            (CredentialKind::Litellm, Some(u)) => Some(validate_base_url(u)?),
            (CredentialKind::Litellm, None) => {
                return Err(VaultError::Invalid(
                    "a LiteLLM credential needs a base URL".into(),
                ));
            }
            (_, Some(u)) => Some(validate_base_url(u)?),
            (_, None) => None,
        };
        let env_var = match (
            req.kind,
            req.env_var
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            (CredentialKind::Other, Some(v)) => {
                if !valid_env_name(v) {
                    return Err(VaultError::Invalid(
                        "env var must match [A-Z_][A-Z0-9_]*".into(),
                    ));
                }
                if reserved_env(v) {
                    return Err(VaultError::Invalid(format!(
                        "{v} is reserved and cannot be set by a credential"
                    )));
                }
                Some(v.to_string())
            }
            (CredentialKind::Other, None) => {
                return Err(VaultError::Invalid(
                    "a custom credential needs an env var name".into(),
                ));
            }
            (_, _) => None,
        };
        let id = CredentialId::new();
        let (ciphertext, nonce) = self.key.seal(user, id, secret.as_bytes())?;
        let summary = self
            .store
            .insert_credential(NewCredential {
                id,
                user_id: user,
                kind: req.kind,
                name: name.to_string(),
                ciphertext,
                nonce: nonce.to_vec(),
                masked: mask(secret),
                base_url,
                env_var,
                expires_at: None,
            })
            .await?;
        tracing::info!(user = %user, credential = %id, kind = ?req.kind, "credential added");
        Ok(summary)
    }

    /// Replace a credential's secret (and optionally its base URL).
    pub async fn rotate(
        &self,
        user: UserId,
        id: CredentialId,
        req: RotateCredentialRequest,
    ) -> Result<CredentialSummary> {
        let existing = self
            .store
            .get_credential_secret(user, id)
            .await?
            .ok_or(VaultError::NotFound)?;
        let secret = validate_secret(&req.secret)?;
        let base_url = req
            .base_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(validate_base_url)
            .transpose()?;
        let (ciphertext, nonce) = self.key.seal(user, id, secret.as_bytes())?;
        let summary = self
            .store
            .rotate_credential(
                user,
                id,
                &ciphertext,
                &nonce,
                &mask(secret),
                base_url.as_deref(),
            )
            .await?;
        tracing::info!(user = %user, credential = %id, kind = ?existing.kind, "credential rotated");
        Ok(summary)
    }

    /// Revoke a credential (the ciphertext is wiped).
    pub async fn revoke(&self, user: UserId, id: CredentialId) -> Result<()> {
        self.store.revoke_credential(user, id).await?;
        tracing::info!(user = %user, credential = %id, "credential revoked");
        Ok(())
    }

    /// The user's credentials. Never includes secret material.
    pub async fn list(&self, user: UserId) -> Result<Vec<CredentialSummary>> {
        Ok(self.store.list_credentials(user).await?)
    }

    /// Decrypt one of `user`'s credentials. Internal callers only.
    async fn reveal(
        &self,
        user: UserId,
        id: CredentialId,
    ) -> Result<(CredentialKind, Zeroizing<String>, Option<String>)> {
        let s = self
            .store
            .get_credential_secret(user, id)
            .await?
            .ok_or(VaultError::NotFound)?;
        let pt = self.key.open(user, id, &s.ciphertext, &s.nonce)?;
        let text = Zeroizing::new(String::from_utf8(pt.to_vec()).map_err(|_| VaultError::Crypto)?);
        Ok((s.kind, text, s.base_url))
    }

    /// Test a credential against its provider and store the resulting health.
    pub async fn test(&self, user: UserId, id: CredentialId) -> Result<CredentialTestResponse> {
        let (kind, secret, base_url) = self.reveal(user, id).await?;
        let out = self.tester.test(kind, &secret, base_url.as_deref()).await;
        if let Some(h) = &out.health {
            self.store.set_credential_health(user, id, h).await?;
        }
        // Belt and braces: a tester must never echo the secret back.
        let message = Redactor::new([secret.as_str()])
            .redact(&out.message)
            .into_owned();
        Ok(CredentialTestResponse {
            ok: out.ok,
            latency_ms: out.latency_ms,
            message,
        })
    }

    /// Decrypt the user's active credentials into the env vars a worker
    /// container needs, plus a redactor for every value.
    ///
    /// Mapping: ClaudeOauth → `CLAUDE_CODE_OAUTH_TOKEN`, AnthropicApiKey →
    /// `ANTHROPIC_API_KEY`, OpenaiApiKey → `OPENAI_API_KEY`, Litellm →
    /// `LITELLM_BASE_URL` + `LITELLM_API_KEY` (and `OPENAI_BASE_URL` +
    /// `OPENAI_API_KEY` for Codex when the user has no OpenAI key), Other →
    /// its `env_var`. When a user has several credentials of one kind the
    /// oldest wins. A credential that fails to decrypt is skipped, logged
    /// and marked invalid.
    pub async fn dispatch_env(&self, user: UserId) -> Result<DispatchSecrets> {
        let creds = self.store.active_credentials_for_dispatch(user).await?;
        let mut decrypted: Vec<Decrypted> = Vec::new();
        for c in creds {
            match self.key.open(user, c.id, &c.ciphertext, &c.nonce) {
                Ok(pt) => match String::from_utf8(pt.to_vec()) {
                    Ok(s) => decrypted.push(Decrypted {
                        kind: c.kind,
                        secret: Zeroizing::new(s),
                        base_url: c.base_url,
                        env_var: c.env_var,
                    }),
                    Err(_) => {
                        tracing::warn!(credential = %c.id, "credential is not valid UTF-8; skipped")
                    }
                },
                Err(_) => {
                    tracing::warn!(user = %user, credential = %c.id, "credential failed to decrypt; skipped");
                    let health = CredentialHealth::Invalid {
                        error: "cannot be decrypted with the current master key".into(),
                    };
                    let _ = self.store.set_credential_health(user, c.id, &health).await;
                }
            }
        }

        let mut out = DispatchSecrets::default();
        let first = |kind: CredentialKind| decrypted.iter().find(|d| d.kind == kind);
        if let Some(d) = first(CredentialKind::ClaudeOauth) {
            out.set_if_absent(env::CLAUDE_CODE_OAUTH_TOKEN, &d.secret);
        }
        if let Some(d) = first(CredentialKind::AnthropicApiKey) {
            out.set_if_absent(env::ANTHROPIC_API_KEY, &d.secret);
        }
        let has_openai = first(CredentialKind::OpenaiApiKey).is_some();
        if let Some(d) = first(CredentialKind::OpenaiApiKey) {
            out.set_if_absent(env::OPENAI_API_KEY, &d.secret);
        }
        if let Some(d) = first(CredentialKind::Litellm) {
            if let Some(b) = &d.base_url {
                out.set_if_absent(env::LITELLM_BASE_URL, b);
            }
            out.set_if_absent(env::LITELLM_API_KEY, &d.secret);
            if !has_openai {
                if let Some(b) = &d.base_url {
                    out.set_if_absent(env::OPENAI_BASE_URL, b);
                }
                out.set_if_absent(env::OPENAI_API_KEY, &d.secret);
            }
        }
        for d in &decrypted {
            if d.kind == CredentialKind::Other
                && let Some(v) = &d.env_var
                && valid_env_name(v)
                && !reserved_env(v)
            {
                out.set_if_absent(v, &d.secret);
            }
        }
        out.redactor = Redactor::new(decrypted.iter().map(|d| d.secret.as_str()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names() {
        assert!(valid_env_name("GEMINI_API_KEY"));
        assert!(valid_env_name("_X1"));
        assert!(!valid_env_name("1X"));
        assert!(!valid_env_name("lower"));
        assert!(!valid_env_name("A-B"));
        assert!(reserved_env("PATH"));
        assert!(reserved_env("LD_PRELOAD"));
        assert!(reserved_env("KILN_RUN_TOKEN"));
        assert!(!reserved_env("GEMINI_API_KEY"));
    }

    #[test]
    fn base_urls() {
        assert_eq!(
            validate_base_url(" http://litellm:4000/ ").unwrap(),
            "http://litellm:4000"
        );
        assert!(validate_base_url("ftp://x").is_err());
        assert!(validate_base_url("https://").is_err());
        assert!(validate_base_url("http://a b").is_err());
    }

    #[test]
    fn dispatch_debug_hides_values() {
        let d = DispatchSecrets {
            env: vec![("ANTHROPIC_API_KEY".into(), "sk-ant-secret-value".into())],
            ..Default::default()
        };
        let s = format!("{d:?}");
        assert!(s.contains("ANTHROPIC_API_KEY"));
        assert!(!s.contains("sk-ant-secret-value"));
    }
}
