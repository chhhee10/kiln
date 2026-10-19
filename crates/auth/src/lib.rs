//! Kiln authentication (PRD §2, §7.4).
//!
//! * Passwords are hashed with **argon2id** into PHC strings.
//! * Sessions are opaque bearer tokens (32 random bytes, base64url). Only the
//!   sha256 of a token is stored, and lookups go through the hash, so token
//!   comparison never happens on the secret itself.
//! * Self-registration is limited to the allow-listed email domain stored in
//!   `settings_kv.allowed_domain`; admins can create anyone.
//! * The first-run [`Auth::setup`] creates the admin and works only while no
//!   users exist.
//! * Device pairing uses one-time 8-character codes (10 minutes, single use,
//!   stored hashed).

use argon2::password_hash::PasswordHasher;
use argon2::{Algorithm, Argon2, Params, PasswordVerifier, Version};
use base64::Engine;
use chrono::Duration;
use kiln_store::{NewDevice, NewUser, Store, StoreError, UserPatch, setting_keys};
use kiln_types::api::{
    ApiError, CreateUserRequest, LoginRequest, PairCompleteRequest, RegisterRequest, SetupRequest,
    UpdateUserRequest,
};
use kiln_types::{ClientKind, DeviceId, Timestamp, User, UserId, UserLimits, UserRole, UserStatus};
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// Minimum password length in characters.
pub const MIN_PASSWORD_LEN: usize = 10;
/// Maximum password length in bytes (bounds hashing cost).
pub const MAX_PASSWORD_LEN: usize = 1024;
/// Sliding session lifetime.
pub const SESSION_TTL_DAYS: i64 = 30;
/// Pairing code lifetime.
pub const PAIRING_TTL_MINUTES: i64 = 10;
/// Pairing code length.
pub const PAIRING_CODE_LEN: usize = 8;
/// Pairing code alphabet: no `0/O`, `1/I/L` or other look-alikes.
pub const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Authentication errors. [`AuthError::to_api_error`] maps them to the
/// wire-level codes in `kiln_types::api::ApiError`.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Unknown email or wrong password: deliberately indistinguishable.
    #[error("invalid email or password")]
    InvalidCredentials,
    /// The account exists and the password was right, but it is disabled.
    #[error("this account is disabled")]
    Disabled,
    /// Missing, unknown or expired session token.
    #[error("not signed in")]
    Unauthorized,
    /// Signed in but not allowed to do this.
    #[error("{0}")]
    Forbidden(String),
    /// Self-registration with an email outside the allowed domain.
    #[error("email domain is not allowed to register")]
    NotAllowlisted,
    /// First-run setup has not happened yet.
    #[error("first-run setup is required")]
    SetupRequired,
    /// Setup was attempted after users already exist.
    #[error("setup has already been completed")]
    AlreadySetUp,
    /// The pairing code is unknown, expired or already used.
    #[error("pairing code is invalid or expired")]
    InvalidPairingCode,
    /// Input validation failed (weak password, bad email, ...).
    #[error("{0}")]
    Invalid(String),
    /// The email is already registered.
    #[error("an account with this email already exists")]
    EmailTaken,
    /// Password hashing failed (should not happen).
    #[error("password hashing failed")]
    Hash,
    /// Store failure.
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for AuthError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(m) => AuthError::Invalid(m),
            other => AuthError::Store(other),
        }
    }
}

impl AuthError {
    /// Machine-readable API error code.
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::InvalidCredentials
            | AuthError::Unauthorized
            | AuthError::InvalidPairingCode => "unauthorized",
            AuthError::Disabled | AuthError::Forbidden(_) => "forbidden",
            AuthError::NotAllowlisted => "not_allowlisted",
            AuthError::SetupRequired => "setup_required",
            AuthError::AlreadySetUp | AuthError::EmailTaken => "conflict",
            AuthError::Invalid(_) => "invalid",
            AuthError::Store(StoreError::NotFound) => "not_found",
            AuthError::Store(StoreError::Conflict(_)) => "conflict",
            AuthError::Hash | AuthError::Store(_) => "internal",
        }
    }

    /// Convert to the wire error. Internal details are not leaked.
    pub fn to_api_error(&self) -> ApiError {
        let msg = match self {
            AuthError::Hash | AuthError::Store(_) if self.code() == "internal" => {
                "internal error".to_string()
            }
            other => other.to_string(),
        };
        ApiError::new(self.code(), msg)
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        e.to_api_error()
    }
}

/// Result alias for auth operations.
pub type Result<T, E = AuthError> = std::result::Result<T, E>;

// ---------------------------------------------------------------- passwords

/// argon2id cost settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasswordCost {
    /// Memory in KiB.
    pub m_cost: u32,
    /// Iterations.
    pub t_cost: u32,
    /// Parallelism.
    pub p_cost: u32,
}

impl PasswordCost {
    /// Production parameters: 19 MiB, 2 iterations, 1 lane (OWASP baseline
    /// for argon2id).
    pub const DEFAULT: PasswordCost = PasswordCost {
        m_cost: 19 * 1024,
        t_cost: 2,
        p_cost: 1,
    };
    /// Minimal parameters for tests only. Never use in production.
    pub const INSECURE_FAST: PasswordCost = PasswordCost {
        m_cost: 64,
        t_cost: 1,
        p_cost: 1,
    };

    fn hasher(self) -> Result<Argon2<'static>> {
        let params = Params::new(self.m_cost, self.t_cost, self.p_cost, None)
            .map_err(|_| AuthError::Hash)?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for PasswordCost {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Hash a password with production argon2id parameters into a PHC string.
pub fn hash_password(pw: &str) -> Result<String> {
    hash_password_with(pw, PasswordCost::DEFAULT)
}

/// Hash a password with explicit cost parameters.
pub fn hash_password_with(pw: &str, cost: PasswordCost) -> Result<String> {
    let hash = cost
        .hasher()?
        .hash_password(pw.as_bytes())
        .map_err(|_| AuthError::Hash)?;
    Ok(hash.to_string())
}

/// Verify a password against a PHC string (parameters come from the hash).
/// Returns false for malformed hashes.
pub fn verify_password(pw: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(pw.as_bytes(), hash)
        .is_ok()
}

/// Enforce the password policy (≥ 10 characters, bounded length).
pub fn check_password_policy(pw: &str) -> Result<()> {
    if pw.chars().count() < MIN_PASSWORD_LEN {
        return Err(AuthError::Invalid(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if pw.len() > MAX_PASSWORD_LEN {
        return Err(AuthError::Invalid("password is too long".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- tokens

/// A new random bearer token: 32 bytes from the OS-seeded CSPRNG, base64url
/// without padding (43 characters).
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// sha256 hex of a token. This is what the store keeps.
pub fn token_hash(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}

// ---------------------------------------------------------------- emails

/// The domain part of an email ("ada@example.com" → "example.com"), or
/// `None` if the address is malformed.
pub fn email_domain(email: &str) -> Option<&str> {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@')?;
    let valid = !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(char::is_whitespace);
    valid.then_some(domain)
}

/// Whether `email` belongs to the allowed domain(s). `allowed` may hold
/// several comma-separated domains and may prefix them with `@`. Matching is
/// exact (no subdomains) and case-insensitive.
pub fn domain_allowed(email: &str, allowed: &str) -> bool {
    let Some(domain) = email_domain(email) else {
        return false;
    };
    allowed
        .split(',')
        .map(|d| d.trim().trim_start_matches('@'))
        .filter(|d| !d.is_empty())
        .any(|d| d.eq_ignore_ascii_case(domain))
}

/// Normalize and validate an allowed-domain setting.
pub fn normalize_domain(domain: &str) -> Result<String> {
    let parts: Vec<String> = domain
        .split(',')
        .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect();
    let ok = !parts.is_empty()
        && parts.iter().all(|d| {
            email_domain(&format!("x@{d}")).is_some()
                && d.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        });
    if !ok {
        return Err(AuthError::Invalid(
            "allowed domain must look like example.com".into(),
        ));
    }
    Ok(parts.join(","))
}

// ---------------------------------------------------------------- pairing codes

/// A new pairing code: 8 characters from [`PAIRING_ALPHABET`].
pub fn new_pairing_code() -> String {
    let mut rng = rand::rng();
    (0..PAIRING_CODE_LEN)
        .map(|_| PAIRING_ALPHABET[rng.random_range(0..PAIRING_ALPHABET.len())] as char)
        .collect()
}

/// Canonical form of a user-typed pairing code (uppercase, separators removed).
pub fn normalize_pairing_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

// ---------------------------------------------------------------- Auth service

/// Authentication service over the control store.
pub struct Auth {
    store: Store,
    cost: PasswordCost,
    session_ttl: Duration,
    /// Serializes first-run setup so two concurrent requests cannot both
    /// create an admin.
    setup_lock: tokio::sync::Mutex<()>,
    /// Hash verified against when the email is unknown, so login timing does
    /// not reveal which emails exist.
    dummy_hash: OnceLock<String>,
}

impl Auth {
    /// Auth with production password cost and a 30-day sliding session TTL.
    pub fn new(store: Store) -> Self {
        Self::with_cost(store, PasswordCost::DEFAULT)
    }

    /// Auth with explicit password cost (tests use [`PasswordCost::INSECURE_FAST`]).
    pub fn with_cost(store: Store, cost: PasswordCost) -> Self {
        Self {
            store,
            cost,
            session_ttl: Duration::days(SESSION_TTL_DAYS),
            setup_lock: tokio::sync::Mutex::new(()),
            dummy_hash: OnceLock::new(),
        }
    }

    /// The underlying store.
    pub fn store(&self) -> &Store {
        &self.store
    }

    fn hash(&self, pw: &str) -> Result<String> {
        hash_password_with(pw, self.cost)
    }

    fn dummy_verify(&self, pw: &str) {
        let hash = match self.dummy_hash.get() {
            Some(h) => h,
            None => {
                let h = self
                    .hash("kiln-dummy-password-for-timing")
                    .unwrap_or_default();
                self.dummy_hash.get_or_init(|| h)
            }
        };
        let _ = verify_password(pw, hash);
    }

    async fn open_session(
        &self,
        user: &User,
        client: Option<ClientKind>,
        device_name: Option<&str>,
    ) -> Result<String> {
        let device: Option<DeviceId> =
            match (client, device_name.map(str::trim).filter(|n| !n.is_empty())) {
                (Some(kind), Some(name)) => Some(
                    self.store
                        .upsert_device(NewDevice {
                            id: None,
                            user_id: user.id,
                            kind,
                            name: name.to_string(),
                        })
                        .await?
                        .id,
                ),
                _ => None,
            };
        let token = new_token();
        self.store
            .create_session(user.id, &token_hash(&token), device, self.session_ttl)
            .await?;
        Ok(token)
    }

    async fn insert_user(
        &self,
        email: &str,
        name: &str,
        password: &str,
        role: UserRole,
    ) -> Result<User> {
        if email_domain(email).is_none() {
            return Err(AuthError::Invalid("email address is invalid".into()));
        }
        if name.trim().is_empty() {
            return Err(AuthError::Invalid("name is required".into()));
        }
        check_password_policy(password)?;
        let password_hash = self.hash(password)?;
        self.store
            .create_user(NewUser {
                email: email.trim().to_string(),
                name: name.trim().to_string(),
                role,
                password_hash,
                limits: UserLimits::default(),
            })
            .await
            .map_err(|e| match e {
                StoreError::Conflict(_) => AuthError::EmailTaken,
                other => other.into(),
            })
    }

    /// True while no user exists (the first-run wizard must run).
    pub async fn setup_required(&self) -> Result<bool> {
        Ok(self.store.count_users().await? == 0)
    }

    /// The allowed self-registration domain(s), if configured.
    pub async fn allowed_domain(&self) -> Result<Option<String>> {
        Ok(self
            .store
            .get_setting::<String>(setting_keys::ALLOWED_DOMAIN)
            .await?)
    }

    /// First-run setup: create the admin, store the allowed domain and open a
    /// session. Fails with [`AuthError::AlreadySetUp`] once any user exists.
    pub async fn setup(&self, req: SetupRequest) -> Result<(User, String)> {
        let _guard = self.setup_lock.lock().await;
        if !self.setup_required().await? {
            return Err(AuthError::AlreadySetUp);
        }
        let domain = normalize_domain(&req.allowed_domain)?;
        let user = self
            .insert_user(
                &req.admin_email,
                &req.admin_name,
                &req.password,
                UserRole::Admin,
            )
            .await?;
        self.store
            .set_setting(setting_keys::ALLOWED_DOMAIN, &domain)
            .await?;
        self.store
            .set_setting(setting_keys::SETUP_DONE, &true)
            .await?;
        tracing::info!(user = %user.id, "first-run setup completed");
        let token = self.open_session(&user, None, None).await?;
        Ok((user, token))
    }

    /// Email + password login. Unknown email and wrong password produce the
    /// same [`AuthError::InvalidCredentials`] (with comparable timing).
    pub async fn login(&self, req: LoginRequest) -> Result<(User, String)> {
        let Some(user) = self.store.get_user_by_email(&req.email).await? else {
            self.dummy_verify(&req.password);
            return Err(AuthError::InvalidCredentials);
        };
        let hash = self.store.user_password_hash(user.id).await?;
        if !verify_password(&req.password, &hash) {
            tracing::info!(user = %user.id, "login failed: wrong password");
            return Err(AuthError::InvalidCredentials);
        }
        if user.status != UserStatus::Active {
            return Err(AuthError::Disabled);
        }
        let token = self
            .open_session(&user, req.client, req.device_name.as_deref())
            .await?;
        Ok((user, token))
    }

    /// Self-registration for emails in the allowed domain. New users are
    /// members.
    pub async fn register(&self, req: RegisterRequest) -> Result<(User, String)> {
        if self.setup_required().await? {
            return Err(AuthError::SetupRequired);
        }
        let allowed = self
            .allowed_domain()
            .await?
            .ok_or(AuthError::NotAllowlisted)?;
        if !domain_allowed(&req.email, &allowed) {
            return Err(AuthError::NotAllowlisted);
        }
        let user = self
            .insert_user(&req.email, &req.name, &req.password, UserRole::Member)
            .await?;
        let token = self.open_session(&user, None, None).await?;
        Ok((user, token))
    }

    /// Resolve a bearer token to its (active) user and slide the session.
    pub async fn authenticate(&self, token: &str) -> Result<User> {
        if token.is_empty() || token.len() > 256 {
            return Err(AuthError::Unauthorized);
        }
        let (user, sid) = self
            .store
            .session_user(&token_hash(token))
            .await?
            .ok_or(AuthError::Unauthorized)?;
        self.store.extend_session(sid, self.session_ttl).await?;
        Ok(user)
    }

    /// End the session for this token. Unknown tokens are ignored.
    pub async fn logout(&self, token: &str) -> Result<()> {
        self.store.delete_session(&token_hash(token)).await?;
        Ok(())
    }

    /// Admin-only: create a user with any email domain.
    pub async fn create_user(&self, by: &User, req: CreateUserRequest) -> Result<User> {
        require_admin(by)?;
        let user = self
            .insert_user(&req.email, &req.name, &req.password, req.role)
            .await?;
        tracing::info!(by = %by.id, user = %user.id, "user created by admin");
        Ok(user)
    }

    /// Admin-only: update a user's name, role, status, limits or password.
    /// Refuses to demote or disable the last active admin. Changing the
    /// password or disabling revokes the user's sessions.
    pub async fn update_user(&self, by: &User, id: UserId, req: UpdateUserRequest) -> Result<User> {
        require_admin(by)?;
        let target = self.store.get_user(id).await?;
        let loses_admin = target.is_admin()
            && target.status == UserStatus::Active
            && (req.role.is_some_and(|r| r != UserRole::Admin)
                || req.status == Some(UserStatus::Disabled));
        if loses_admin {
            let other_admins = self
                .store
                .list_users()
                .await?
                .iter()
                .filter(|u| u.id != id && u.is_admin() && u.status == UserStatus::Active)
                .count();
            if other_admins == 0 {
                return Err(AuthError::Forbidden(
                    "cannot remove the last active admin".into(),
                ));
            }
        }
        if let Some(l) = &req.limits
            && (!(1..=8).contains(&l.concurrency)
                || !(0.0..=1.0).contains(&l.ram_share)
                || !l.monthly_budget_usd.is_finite()
                || l.monthly_budget_usd < 0.0)
        {
            return Err(AuthError::Invalid("limits out of range".into()));
        }
        let password_hash = match &req.password {
            Some(pw) => {
                check_password_policy(pw)?;
                Some(self.hash(pw)?)
            }
            None => None,
        };
        let revoke = password_hash.is_some();
        let user = self
            .store
            .update_user(
                id,
                UserPatch {
                    name: req.name,
                    role: req.role,
                    status: req.status,
                    limits: req.limits,
                    password_hash,
                },
            )
            .await?;
        if revoke {
            self.store.delete_user_sessions(id).await?;
        }
        Ok(user)
    }

    /// Change one's own password (requires the current one). Other sessions
    /// are revoked; a fresh token is returned.
    pub async fn change_password(&self, user: &User, current: &str, new: &str) -> Result<String> {
        let hash = self.store.user_password_hash(user.id).await?;
        if !verify_password(current, &hash) {
            return Err(AuthError::InvalidCredentials);
        }
        check_password_policy(new)?;
        let password_hash = Some(self.hash(new)?);
        self.store
            .update_user(
                user.id,
                UserPatch {
                    password_hash,
                    ..Default::default()
                },
            )
            .await?;
        self.store.delete_user_sessions(user.id).await?;
        self.open_session(user, None, None).await
    }

    /// Admin-only: set the allowed self-registration domain.
    pub async fn set_allowed_domain(&self, by: &User, domain: &str) -> Result<String> {
        require_admin(by)?;
        let d = normalize_domain(domain)?;
        self.store
            .set_setting(setting_keys::ALLOWED_DOMAIN, &d)
            .await?;
        Ok(d)
    }

    /// Start pairing a new device for `user`: returns the one-time code and
    /// its expiry (10 minutes).
    pub async fn start_pairing(&self, user: &User) -> Result<(String, Timestamp)> {
        if user.status != UserStatus::Active {
            return Err(AuthError::Disabled);
        }
        let code = new_pairing_code();
        let expires = self
            .store
            .create_pairing_code(
                &token_hash(&code),
                user.id,
                Duration::minutes(PAIRING_TTL_MINUTES),
            )
            .await?;
        Ok((code, expires))
    }

    /// Redeem a pairing code: registers the device and opens a session.
    pub async fn complete_pairing(&self, req: PairCompleteRequest) -> Result<(User, String)> {
        let code = normalize_pairing_code(&req.code);
        if code.len() != PAIRING_CODE_LEN {
            return Err(AuthError::InvalidPairingCode);
        }
        let user_id = self
            .store
            .consume_pairing_code(&token_hash(&code))
            .await?
            .ok_or(AuthError::InvalidPairingCode)?;
        let user = self.store.get_user(user_id).await?;
        if user.status != UserStatus::Active {
            return Err(AuthError::Disabled);
        }
        let name = if req.device_name.trim().is_empty() {
            "paired device"
        } else {
            req.device_name.as_str()
        };
        let token = self
            .open_session(&user, Some(req.client), Some(name))
            .await?;
        Ok((user, token))
    }
}

fn require_admin(by: &User) -> Result<()> {
    if by.is_admin() && by.status == UserStatus::Active {
        Ok(())
    } else {
        Err(AuthError::Forbidden("admin only".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash_password_with("correct horse battery", PasswordCost::INSECURE_FAST).unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("wrong horse battery", &h));
        assert!(!verify_password("x", "not a hash"));
    }

    #[test]
    fn production_params_are_argon2id() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{h}");
        assert!(verify_password("correct horse battery", &h));
    }

    #[test]
    fn tokens() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn domains() {
        assert_eq!(email_domain("ada@example.com"), Some("example.com"));
        assert_eq!(email_domain("a@b@example.com"), Some("example.com"));
        assert_eq!(email_domain("@example.com"), None);
        assert_eq!(email_domain("ada@"), None);
        assert_eq!(email_domain("ada@localhost"), None);
        assert_eq!(email_domain("ada example.com"), None);
        assert!(domain_allowed("ada@Example.COM", "example.com"));
        assert!(domain_allowed("ada@example.com", "@example.com"));
        assert!(domain_allowed("ada@other.org", "example.com, other.org"));
        assert!(!domain_allowed("ada@sub.example.com", "example.com"));
        assert!(!domain_allowed("ada@example.com.evil.io", "example.com"));
        assert!(!domain_allowed("ada@evilexample.com", "example.com"));
        assert!(!domain_allowed("ada@example.com", ""));
        assert_eq!(normalize_domain(" @Example.com ").unwrap(), "example.com");
        assert!(normalize_domain("not a domain").is_err());
        assert!(normalize_domain("").is_err());
    }

    #[test]
    fn pairing_codes() {
        for _ in 0..100 {
            let c = new_pairing_code();
            assert_eq!(c.len(), PAIRING_CODE_LEN);
            assert!(c.bytes().all(|b| PAIRING_ALPHABET.contains(&b)));
            assert!(!c.contains(['0', 'O', '1', 'I', 'L']));
        }
        assert_eq!(normalize_pairing_code("abcd-efgh"), "ABCDEFGH");
        assert_eq!(normalize_pairing_code(" ab cd ef gh "), "ABCDEFGH");
    }

    #[test]
    fn password_policy() {
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy("exactly10!").is_ok());
        assert!(check_password_policy(&"x".repeat(2000)).is_err());
    }

    #[test]
    fn api_errors_do_not_leak_internals() {
        let e = AuthError::Store(StoreError::Corrupt("secret detail".into()));
        let api = e.to_api_error();
        assert_eq!(api.code, "internal");
        assert!(!api.error.contains("secret"));
        assert_eq!(
            AuthError::NotAllowlisted.to_api_error().code,
            "not_allowlisted"
        );
    }
}
