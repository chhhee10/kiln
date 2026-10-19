//! `KILN_MASTER_KEY` overrides every other key source. Kept in its own test
//! binary (one test) because it mutates the process environment.

use base64::Engine;
use kiln_vault::*;

#[test]
fn env_master_key_overrides_file_and_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let raw = [7u8; 32];
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
    // SAFETY: this binary runs a single test, so no other thread reads the
    // environment concurrently.
    unsafe { std::env::set_var(ENV_MASTER_KEY, &b64) };
    let (k, src) = MasterKey::load_or_create_with(
        dir.path(),
        KeyOptions {
            env: true,
            keychain: true,
        },
    )
    .unwrap();
    assert_eq!(src, KeySource::Env);
    assert!(
        !dir.path().join(KEY_FILE).exists(),
        "no key file is written when the env key is used"
    );

    // Same bytes as from_bytes → interoperable ciphertexts.
    let other = MasterKey::from_bytes(raw);
    let (u, c) = (kiln_types::UserId::new(), kiln_types::CredentialId::new());
    let (ct, nonce) = k.seal(u, c, b"secret").unwrap();
    assert_eq!(other.open(u, c, &ct, &nonce).unwrap().as_slice(), b"secret");

    // A malformed override is an error, not a silent fallback.
    unsafe { std::env::set_var(ENV_MASTER_KEY, "not-base64!!") };
    assert!(
        MasterKey::load_or_create_with(
            dir.path(),
            KeyOptions {
                env: true,
                keychain: false
            }
        )
        .is_err()
    );
    unsafe { std::env::remove_var(ENV_MASTER_KEY) };
}
