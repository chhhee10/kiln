//! The vault master key and the AEAD built on it.

use crate::{Result, VaultError};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use kiln_types::{CredentialId, UserId};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// Keychain service name.
pub const KEYCHAIN_SERVICE: &str = "kiln";
/// Keychain account name.
pub const KEYCHAIN_ACCOUNT: &str = "master-key";
/// Environment override: the master key as base64 (32 bytes). For tests/CI.
pub const ENV_MASTER_KEY: &str = "KILN_MASTER_KEY";
/// Environment switch: when set to `1`/`true`, never touch the OS keychain.
pub const ENV_NO_KEYCHAIN: &str = "KILN_NO_KEYCHAIN";
/// File name of the fallback key under the data dir.
pub const KEY_FILE: &str = "master.key";
/// AEAD nonce length (96 bits).
pub const NONCE_LEN: usize = 12;

/// Where a loaded master key came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// `KILN_MASTER_KEY` environment variable.
    Env,
    /// OS keychain (macOS Keychain / Secret Service / Windows Credential Manager).
    Keychain,
    /// `<data_dir>/master.key`.
    File,
}

/// Which sources [`MasterKey::load_or_create_with`] may use.
#[derive(Clone, Copy, Debug)]
pub struct KeyOptions {
    /// Honour `KILN_MASTER_KEY`.
    pub env: bool,
    /// Try the OS keychain.
    pub keychain: bool,
}

impl Default for KeyOptions {
    fn default() -> Self {
        let no_keychain =
            std::env::var(ENV_NO_KEYCHAIN).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        Self {
            env: true,
            keychain: !no_keychain,
        }
    }
}

/// 256-bit master key. Zeroized on drop; never printed.
#[derive(Clone)]
pub struct MasterKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

impl MasterKey {
    /// Wrap raw key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// A fresh random key from the OS-seeded CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let k = Self::from_bytes(bytes);
        bytes.zeroize();
        k
    }

    /// Parse a base64-encoded 32-byte key (surrounding whitespace ignored).
    pub fn from_base64(s: &str) -> Result<Self> {
        let raw = Zeroizing::new(
            b64()
                .decode(s.trim())
                .map_err(|_| VaultError::BadKey("not valid base64".into()))?,
        );
        let arr: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| VaultError::BadKey(format!("expected 32 bytes, got {}", raw.len())))?;
        Ok(Self::from_bytes(arr))
    }

    /// Base64 encoding of the key, for storage in the keychain or key file.
    fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(b64().encode(self.0.as_slice()))
    }

    /// Load the master key, creating one on first run.
    ///
    /// Order: `KILN_MASTER_KEY` (base64) → OS keychain (service `kiln`,
    /// account `master-key`) → `<data_dir>/master.key` (0600). An existing
    /// key file always wins over generating a new keychain entry, so a key
    /// created while the keychain was unavailable is never orphaned. If the
    /// keychain is unavailable or errors (common on headless Linux) the file
    /// is used and a warning is logged. Set `KILN_NO_KEYCHAIN=1` to skip the
    /// keychain entirely.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        Self::load_or_create_with(data_dir, KeyOptions::default()).map(|(k, _)| k)
    }

    /// [`MasterKey::load_or_create`] with explicit sources; also reports
    /// where the key came from.
    pub fn load_or_create_with(data_dir: &Path, opts: KeyOptions) -> Result<(Self, KeySource)> {
        if opts.env
            && let Ok(v) = std::env::var(ENV_MASTER_KEY)
            && !v.trim().is_empty()
        {
            return Ok((Self::from_base64(&v)?, KeySource::Env));
        }
        let file = data_dir.join(KEY_FILE);
        if opts.keychain {
            match keychain_load_or_create(&file) {
                Ok(Some(k)) => return Ok((k, KeySource::Keychain)),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, path = %file.display(),
                        "OS keychain unavailable; using the master key file instead");
                }
            }
        }
        Ok((load_or_create_file(&file)?, KeySource::File))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        // A 32-byte slice always converts into a ChaCha20 key.
        let key = Key::from(*self.0);
        ChaCha20Poly1305::new(&key)
    }

    /// Encrypt `plaintext` with a fresh random 96-bit nonce, binding it to
    /// `user || credential` as associated data. Returns `(ciphertext, nonce)`.
    pub fn seal(
        &self,
        user: UserId,
        cred: CredentialId,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, [u8; NONCE_LEN])> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let aad = aad(user, cred);
        let ct = self
            .cipher()
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| VaultError::Crypto)?;
        Ok((ct, nonce))
    }

    /// Decrypt a ciphertext sealed for `user || credential`. Fails if the key,
    /// nonce, ciphertext or either id differ.
    pub fn open(
        &self,
        user: UserId,
        cred: CredentialId,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>> {
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| VaultError::Crypto)?;
        let aad = aad(user, cred);
        self.cipher()
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| VaultError::Crypto)
    }
}

/// AEAD associated data: the 16 user-id bytes followed by the 16
/// credential-id bytes.
fn aad(user: UserId, cred: CredentialId) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(user.as_uuid().as_bytes());
    out[16..].copy_from_slice(cred.as_uuid().as_bytes());
    out
}

/// `Ok(Some)` when the keychain holds (or now holds) the key, `Ok(None)` when
/// the file fallback should be used, `Err` when the keychain failed.
fn keychain_load_or_create(file: &Path) -> std::result::Result<Option<MasterKey>, String> {
    let entry =
        keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(s) => {
            let s = Zeroizing::new(s);
            MasterKey::from_base64(&s)
                .map(Some)
                .map_err(|e| format!("keychain entry is invalid: {e}"))
        }
        Err(keyring::Error::NoEntry) => {
            if file.exists() {
                // A key created earlier without a keychain stays authoritative.
                return Ok(None);
            }
            let key = MasterKey::generate();
            entry
                .set_password(&key.to_base64())
                .map_err(|e| e.to_string())?;
            tracing::info!("created a new vault master key in the OS keychain");
            Ok(Some(key))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn load_or_create_file(path: &PathBuf) -> Result<MasterKey> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let s = Zeroizing::new(s);
            check_permissions(path);
            MasterKey::from_base64(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let key = MasterKey::generate();
            write_key_file(path, &key)?;
            tracing::warn!(path = %path.display(),
                "created a new vault master key file (no OS keychain); protect and back up this file");
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(unix)]
fn write_key_file(path: &Path, key: &MasterKey) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(key.to_base64().as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_key_file(path: &Path, key: &MasterKey) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(key.to_base64().as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn check_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "master key file is readable by group/others; chmod 600 it");
    }
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_ONLY: KeyOptions = KeyOptions {
        env: false,
        keychain: false,
    };

    #[test]
    fn seal_open_roundtrip_and_aad_binding() {
        let k = MasterKey::generate();
        let (u, c) = (UserId::new(), CredentialId::new());
        let (ct, nonce) = k.seal(u, c, b"sk-ant-secret").unwrap();
        assert_ne!(ct.as_slice(), b"sk-ant-secret");
        assert_eq!(
            k.open(u, c, &ct, &nonce).unwrap().as_slice(),
            b"sk-ant-secret"
        );
        // Different user, credential, key or nonce → fails.
        assert!(k.open(UserId::new(), c, &ct, &nonce).is_err());
        assert!(k.open(u, CredentialId::new(), &ct, &nonce).is_err());
        assert!(MasterKey::generate().open(u, c, &ct, &nonce).is_err());
        assert!(k.open(u, c, &ct, &[0u8; 12]).is_err());
        assert!(k.open(u, c, &ct, &[0u8; 3]).is_err());
        // Nonces are random.
        let (_, n2) = k.seal(u, c, b"sk-ant-secret").unwrap();
        assert_ne!(nonce, n2);
    }

    #[test]
    fn file_key_created_with_0600_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let (k1, src) = MasterKey::load_or_create_with(dir.path(), FILE_ONLY).unwrap();
        assert_eq!(src, KeySource::File);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let (k2, _) = MasterKey::load_or_create_with(dir.path(), FILE_ONLY).unwrap();
        assert_eq!(*k1.0, *k2.0);
    }

    #[test]
    fn creates_missing_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        MasterKey::load_or_create_with(&nested, FILE_ONLY).unwrap();
        assert!(nested.join(KEY_FILE).exists());
    }

    #[test]
    fn corrupt_key_file_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(KEY_FILE), "garbage").unwrap();
        assert!(MasterKey::load_or_create_with(dir.path(), FILE_ONLY).is_err());
    }

    #[test]
    fn base64_parsing() {
        let k = MasterKey::generate();
        let s = k.to_base64();
        assert_eq!(
            *MasterKey::from_base64(&format!("  {}\n", s.as_str()))
                .unwrap()
                .0,
            *k.0
        );
        assert!(MasterKey::from_base64("AAAA").is_err());
        assert!(MasterKey::from_base64("!!!").is_err());
        assert_eq!(format!("{k:?}"), "MasterKey(<redacted>)");
    }
}
