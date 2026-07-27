//! Envelope encryption for data at rest.
//!
//! # The pattern
//!
//! Rather than encrypting every record with one master key, we use two layers:
//!
//!   - A **KEK** (key-encryption key) lives in config / a real KMS. It never
//!     encrypts data directly and never touches the database.
//!   - A **DEK** (data-encryption key) is generated per record, encrypts that
//!     record, and is itself encrypted by the KEK. The wrapped DEK is stored
//!     alongside the ciphertext.
//!
//! # Why bother in a sandbox
//!
//! Two reasons. Rotating the KEK only requires re-wrapping DEKs, not
//! re-encrypting every record — that's the operational payoff, and it's the
//! reason real systems do this. And a reader learning from this codebase sees
//! the shape a real vault has, rather than a naive single-key scheme they'd
//! then copy into production.
//!
//! We use AES-256-GCM: authenticated encryption, so tampering with ciphertext
//! is detected on decrypt rather than silently producing garbage.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("key must be exactly 32 bytes (64 hex chars), got {0}")]
    InvalidKeyLength(usize),

    #[error("key is not valid hex")]
    InvalidKeyEncoding,

    #[error("encryption failed")]
    EncryptFailed,

    #[error("decryption failed — wrong key or tampered ciphertext")]
    DecryptFailed,

    #[error("ciphertext is malformed")]
    MalformedCiphertext,
}

/// AES-GCM nonce size, in bytes. 96 bits is the standard for GCM.
const NONCE_LEN: usize = 12;

/// A record encrypted under its own DEK, with that DEK wrapped by the KEK.
/// Both fields are safe to store in the database — neither is usable without
/// the KEK, which lives only in config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Nonce ‖ ciphertext of the payload, under the DEK.
    pub ciphertext: Vec<u8>,
    /// Nonce ‖ ciphertext of the DEK itself, under the KEK.
    pub wrapped_dek: Vec<u8>,
}

/// Parse a 64-char hex string into a 32-byte AES-256 key.
fn parse_key(hex_key: &str) -> Result<[u8; 32], EncryptionError> {
    let bytes = hex::decode(hex_key).map_err(|_| EncryptionError::InvalidKeyEncoding)?;
    if bytes.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength(bytes.len()));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Encrypt `plaintext` under a fresh random key, prefixing the nonce.
fn seal(key_bytes: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    let key = Key::<Aes256Gcm>::from(*key_bytes);
    let cipher = Aes256Gcm::new(&key);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| EncryptionError::EncryptFailed)?;

    // Prepend the nonce so decrypt can recover it. Nonces are not secret.
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn open(key_bytes: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    if sealed.len() < NONCE_LEN {
        return Err(EncryptionError::MalformedCiphertext);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);

    let key = Key::<Aes256Gcm>::from(*key_bytes);
    let cipher = Aes256Gcm::new(&key);
    let nonce_bytes: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .expect("split_at guarantees exactly NONCE_LEN bytes");
    let nonce = Nonce::from(nonce_bytes);

    cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| EncryptionError::DecryptFailed)
}

/// Encrypt a payload: generate a DEK, encrypt the data with it, wrap the DEK
/// with the KEK. Both outputs go to the database.
pub fn encrypt(kek_hex: &str, plaintext: &[u8]) -> Result<Envelope, EncryptionError> {
    let kek = parse_key(kek_hex)?;

    // Fresh DEK for this record.
    let mut dek = [0u8; 32];
    OsRng.fill_bytes(&mut dek);

    let ciphertext = seal(&dek, plaintext)?;
    let wrapped_dek = seal(&kek, &dek)?;

    Ok(Envelope {
        ciphertext,
        wrapped_dek,
    })
}

/// Decrypt: unwrap the DEK with the KEK, then decrypt the payload with the DEK.
pub fn decrypt(kek_hex: &str, envelope: &Envelope) -> Result<Vec<u8>, EncryptionError> {
    let kek = parse_key(kek_hex)?;

    let dek_bytes = open(&kek, &envelope.wrapped_dek)?;
    if dek_bytes.len() != 32 {
        return Err(EncryptionError::MalformedCiphertext);
    }
    let mut dek = [0u8; 32];
    dek.copy_from_slice(&dek_bytes);

    open(&dek, &envelope.ciphertext)
}

/// Re-wrap a record's DEK under a new KEK. This is the key-rotation path — it
/// touches only the wrapped DEK, never the (potentially large) ciphertext,
/// which is the entire point of envelope encryption.
pub fn rewrap(
    old_kek_hex: &str,
    new_kek_hex: &str,
    envelope: &Envelope,
) -> Result<Envelope, EncryptionError> {
    let old_kek = parse_key(old_kek_hex)?;
    let new_kek = parse_key(new_kek_hex)?;

    let dek = open(&old_kek, &envelope.wrapped_dek)?;
    let wrapped_dek = seal(&new_kek, &dek)?;

    Ok(Envelope {
        ciphertext: envelope.ciphertext.clone(),
        wrapped_dek,
    })
}

/// Generate a KEK for config. Run once at setup; store the output in
/// `CRYPTO_MASTER_KEY`.
pub fn generate_kek() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kek() -> String {
        generate_kek()
    }

    #[test]
    fn round_trip() {
        let k = kek();
        let msg = b"sensitive metadata";
        let env = encrypt(&k, msg).unwrap();
        assert_eq!(decrypt(&k, &env).unwrap(), msg);
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let k = kek();
        let msg = b"card_holder_name_jane_doe";
        let env = encrypt(&k, msg).unwrap();
        assert!(!env.ciphertext.windows(msg.len()).any(|w| w == msg));
    }

    #[test]
    fn each_encryption_uses_a_fresh_dek_and_nonce() {
        let k = kek();
        let a = encrypt(&k, b"same message").unwrap();
        let b = encrypt(&k, b"same message").unwrap();
        // Identical plaintext must not produce identical ciphertext.
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_ne!(a.wrapped_dek, b.wrapped_dek);
    }

    #[test]
    fn wrong_kek_fails_to_decrypt() {
        let env = encrypt(&kek(), b"secret").unwrap();
        let err = decrypt(&kek(), &env).unwrap_err();
        assert!(matches!(err, EncryptionError::DecryptFailed));
    }

    #[test]
    fn tampered_ciphertext_is_detected() {
        // GCM is authenticated: a flipped bit must fail, not decrypt to garbage.
        let k = kek();
        let mut env = encrypt(&k, b"transfer 100").unwrap();
        let last = env.ciphertext.len() - 1;
        env.ciphertext[last] ^= 0x01;
        assert!(decrypt(&k, &env).is_err());
    }

    #[test]
    fn tampered_wrapped_dek_is_detected() {
        let k = kek();
        let mut env = encrypt(&k, b"data").unwrap();
        let last = env.wrapped_dek.len() - 1;
        env.wrapped_dek[last] ^= 0x01;
        assert!(decrypt(&k, &env).is_err());
    }

    #[test]
    fn rotation_rewraps_without_touching_ciphertext() {
        let old = kek();
        let new = kek();
        let msg = b"long lived record";

        let env = encrypt(&old, msg).unwrap();
        let rotated = rewrap(&old, &new, &env).unwrap();

        // The payload ciphertext is untouched — only the tiny DEK was re-wrapped.
        assert_eq!(rotated.ciphertext, env.ciphertext);
        assert_ne!(rotated.wrapped_dek, env.wrapped_dek);

        // Readable under the new KEK, not the old one.
        assert_eq!(decrypt(&new, &rotated).unwrap(), msg);
        assert!(decrypt(&old, &rotated).is_err());
    }

    #[test]
    fn malformed_keys_rejected() {
        assert!(matches!(
            encrypt("tooshort", b"x"),
            Err(EncryptionError::InvalidKeyLength(_))
        ));
        assert!(matches!(
            encrypt("zz".repeat(32).as_str(), b"x"),
            Err(EncryptionError::InvalidKeyEncoding)
        ));
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let k = kek();
        let env = encrypt(&k, b"").unwrap();
        assert_eq!(decrypt(&k, &env).unwrap(), b"");
    }
}
