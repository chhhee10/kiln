-- Web Push subscription keys (RFC 8291): push payloads to a device that
-- registered them are encrypted, so the push relay only sees ciphertext.
ALTER TABLE devices ADD COLUMN push_p256dh TEXT;  -- base64url P-256 public key
ALTER TABLE devices ADD COLUMN push_auth   TEXT;  -- base64url 16-byte auth secret
