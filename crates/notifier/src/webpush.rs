//! Web Push message encryption (RFC 8291 over RFC 8188 `aes128gcm`).
//!
//! The device registers its P-256 public key (`p256dh`) and a 16-byte auth
//! secret with its endpoint; payloads are then encrypted to it, so the push
//! relay (ntfy, Google's FCM bridge, …) only sees ciphertext. The UnifiedPush
//! connector decrypts on the phone.

use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2_10::Sha256;

/// Record size advertised in the header. One record carries the payload.
const RECORD_SIZE: u32 = 4096;
/// Largest plaintext that fits one record (tag + delimiter take 17 bytes).
pub const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 17;

/// The device's subscription keys, base64url (padding optional).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebPushError {
    #[error("invalid subscription key: {0}")]
    BadKey(&'static str),
    #[error("payload too large for one record ({0} bytes)")]
    TooLarge(usize),
}

fn b64(s: &str) -> Result<Vec<u8>, WebPushError> {
    let s = s.trim();
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(s))
        .map_err(|_| WebPushError::BadKey("not base64url"))
}

/// Check keys at registration time so a bad key is rejected up front.
pub fn validate_keys(keys: &SubscriptionKeys) -> Result<(), WebPushError> {
    PublicKey::from_sec1_bytes(&b64(&keys.p256dh)?)
        .map_err(|_| WebPushError::BadKey("p256dh is not a P-256 point"))?;
    if b64(&keys.auth)?.len() != 16 {
        return Err(WebPushError::BadKey("auth must be 16 bytes"));
    }
    Ok(())
}

/// Encrypt `plaintext` for the subscription: the request body to POST with
/// `Content-Encoding: aes128gcm`.
pub fn encrypt(keys: &SubscriptionKeys, plaintext: &[u8]) -> Result<Vec<u8>, WebPushError> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    encrypt_with(&SecretKey::random(&mut OsRng), salt, keys, plaintext)
}

/// [`encrypt`] with a fixed application-server key and salt (tests).
fn encrypt_with(
    as_secret: &SecretKey,
    salt: [u8; 16],
    keys: &SubscriptionKeys,
    plaintext: &[u8],
) -> Result<Vec<u8>, WebPushError> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(WebPushError::TooLarge(plaintext.len()));
    }
    let ua_bytes = b64(&keys.p256dh)?;
    let ua_public = PublicKey::from_sec1_bytes(&ua_bytes)
        .map_err(|_| WebPushError::BadKey("p256dh is not a P-256 point"))?;
    let auth = b64(&keys.auth)?;
    if auth.len() != 16 {
        return Err(WebPushError::BadKey("auth must be 16 bytes"));
    }
    let as_public = as_secret.public_key().to_encoded_point(false);
    let as_public = as_public.as_bytes();
    let ua_public_bytes = ua_public.to_encoded_point(false);

    // RFC 8291 §3.4: combine the ECDH secret with the auth secret.
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public_bytes.as_bytes());
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .expect("32 bytes is a valid HKDF length");

    // RFC 8188 §2.2/2.3: content-encryption key and nonce.
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .expect("valid length");
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .expect("valid length");

    let mut record = plaintext.to_vec();
    record.push(0x02); // last-record delimiter, no padding
    let ciphertext = Aes128Gcm::new(&cek.into())
        .encrypt(&Nonce::from(nonce), record.as_slice())
        .expect("AES-GCM encryption of one record cannot fail");

    let mut out = Vec::with_capacity(16 + 4 + 1 + as_public.len() + ciphertext.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        b64(s).unwrap()
    }

    /// RFC 8291 Appendix A.
    #[test]
    fn rfc8291_test_vector() {
        let as_secret =
            SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let keys = SubscriptionKeys {
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
        };
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt_with(
            &as_secret,
            salt,
            &keys,
            b"When I grow up, I want to be a watermelon",
        )
        .unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    /// The receiving side of RFC 8291, for round-trip checks.
    fn decrypt(ua: &SecretKey, auth: &[u8], body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let idlen = body[20] as usize;
        let as_public = PublicKey::from_sec1_bytes(&body[21..21 + idlen]).unwrap();
        let ct = &body[21 + idlen..];
        let shared = p256::ecdh::diffie_hellman(ua.to_nonzero_scalar(), as_public.as_affine());
        let mut info = b"WebPush: info\0".to_vec();
        info.extend_from_slice(ua.public_key().to_encoded_point(false).as_bytes());
        info.extend_from_slice(&body[21..21 + idlen]);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
            .expand(&info, &mut ikm)
            .unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .unwrap();
        let mut pt = Aes128Gcm::new(&cek.into())
            .decrypt(&Nonce::from(nonce), ct)
            .unwrap();
        assert_eq!(pt.pop(), Some(0x02));
        pt
    }

    #[test]
    fn round_trips_through_a_receiver() {
        let ua = SecretKey::random(&mut OsRng);
        let auth = [9u8; 16];
        let keys = SubscriptionKeys {
            p256dh: URL_SAFE_NO_PAD.encode(ua.public_key().to_encoded_point(false).as_bytes()),
            auth: URL_SAFE.encode(auth),
        };
        let msg = br#"{"title":"QA failed"}"#;
        assert_eq!(decrypt(&ua, &auth, &encrypt(&keys, msg).unwrap()), msg);
    }

    #[test]
    fn validates_keys_and_size() {
        let ua = SecretKey::random(&mut OsRng);
        let good = SubscriptionKeys {
            p256dh: URL_SAFE_NO_PAD.encode(ua.public_key().to_encoded_point(false).as_bytes()),
            auth: URL_SAFE_NO_PAD.encode([1u8; 16]),
        };
        validate_keys(&good).unwrap();
        let body = encrypt(&good, b"{}").unwrap();
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + 2 + 1 + 16);
        assert_ne!(
            encrypt(&good, b"{}").unwrap(),
            body,
            "fresh salt and key each time"
        );
        assert_eq!(
            encrypt(&good, &vec![0; MAX_PLAINTEXT + 1]),
            Err(WebPushError::TooLarge(MAX_PLAINTEXT + 1))
        );
        let bad_auth = SubscriptionKeys {
            auth: "AAAA".into(),
            ..good.clone()
        };
        assert!(validate_keys(&bad_auth).is_err());
        let bad_key = SubscriptionKeys {
            p256dh: URL_SAFE_NO_PAD.encode([4u8; 65]),
            ..good
        };
        assert!(validate_keys(&bad_key).is_err());
    }
}
