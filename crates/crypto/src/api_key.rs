//! API key generation and verification.
//!
//! Keys have the form `{prefix}_{random}` — e.g. `sk_test_a1b2c3...`. The
//! plaintext secret is shown to the merchant EXACTLY ONCE at creation; only an
//! Argon2id hash is stored, so a database dump cannot be used to impersonate a
//! merchant.
//!
//! # Why Argon2id and not SHA-256
//!
//! API keys are high-entropy random strings, so a fast hash would arguably be
//! defensible. We use Argon2id anyway because it costs us nothing at our
//! request volume (mitigated by the verified-key cache in the auth middleware)
//! and it means the sandbox demonstrates the right practice — someone reading
//! this code to learn should see the correct answer, not the defensible
//! shortcut.

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use rand::RngCore;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeyError {
    #[error("failed to hash key: {0}")]
    Hashing(String),

    #[error("stored hash is malformed: {0}")]
    MalformedHash(String),
}

/// A freshly generated key. `plaintext` is returned to the caller ONCE and
/// must never be persisted; `hash` is what goes in the database.
#[derive(Debug, Clone)]
pub struct GeneratedKey {
    pub plaintext: String,
    pub hash: String,
}

/// Number of random bytes in the key body. 32 bytes = 256 bits of entropy,
/// rendered as 64 hex chars. Far beyond brute-force reach.
const KEY_BYTES: usize = 32;

/// Generate a key with the given prefix (`"sk_test"` or `"pk_test"`).
///
/// The `_test` suffix in every prefix is a deliberate safety property: the
/// server asserts at boot that no key lacks it, so a sandbox key can never be
/// mistaken for a live credential.
pub fn generate(prefix: &str) -> Result<GeneratedKey, KeyError> {
    let mut bytes = [0u8; KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let body = hex::encode(bytes);
    let plaintext = format!("{prefix}_{body}");
    let hash = lookup_hash(&plaintext);

    Ok(GeneratedKey { plaintext, hash })
}

/// Deterministic lookup hash for API tokens: SHA-256, hex encoded.
///
/// # Why SHA-256 here when passwords get Argon2
///
/// Argon2's deliberate slowness defends LOW-entropy secrets (passwords)
/// against brute force. API keys carry 256 bits of entropy — brute force is
/// already impossible — and the auth middleware must look a presented token
/// up by index, which a salted hash can never support (you'd have to run
/// Argon2 against every row). SHA-256-of-token is the standard design for
/// API credentials. The Argon2 functions below remain for dashboard
/// passwords, where they are the right tool.
pub fn lookup_hash(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

/// Argon2id hash of a key's plaintext.
pub fn hash(plaintext: &str) -> Result<String, KeyError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| KeyError::Hashing(e.to_string()))
}

/// Verify a presented key against a stored hash. Returns `false` for a
/// mismatch, `Err` only if the stored hash itself is unparseable.
///
/// Argon2's verifier is constant-time internally, so this does not leak timing
/// information about how much of the key matched.
pub fn verify(plaintext: &str, stored_hash: &str) -> Result<bool, KeyError> {
    let parsed =
        PasswordHash::new(stored_hash).map_err(|e| KeyError::MalformedHash(e.to_string()))?;

    Ok(Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed)
        .is_ok())
}

/// Split a presented key into its prefix and body, for routing to the right
/// lookup (`sk_test` vs `pk_test`). Returns `None` if the shape is wrong.
pub fn split_prefix(key: &str) -> Option<(&str, &str)> {
    // Prefixes contain an underscore themselves ("sk_test"), so split from the
    // right on the LAST underscore.
    let idx = key.rfind('_')?;
    let (prefix, rest) = key.split_at(idx);
    let body = &rest[1..];
    if prefix.is_empty() || body.is_empty() {
        return None;
    }
    Some((prefix, body))
}

/// Reject any key that isn't a test key. Called at server boot as a hard
/// safety assertion — the sandbox must never hold a credential shaped like a
/// production one.
pub fn is_test_key(key: &str) -> bool {
    key.starts_with("sk_test_") || key.starts_with("pk_test_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_key_has_correct_shape() {
        let k = generate("sk_test").unwrap();
        assert!(k.plaintext.starts_with("sk_test_"));
        // prefix (7) + underscore (1) + 64 hex chars
        assert_eq!(k.plaintext.len(), 7 + 1 + 64);
    }

    #[test]
    fn generated_keys_are_unique() {
        let a = generate("sk_test").unwrap();
        let b = generate("sk_test").unwrap();
        assert_ne!(a.plaintext, b.plaintext);
    }

    #[test]
    fn hash_is_not_the_plaintext() {
        let k = generate("sk_test").unwrap();
        assert_ne!(k.hash, k.plaintext);
        assert!(!k.hash.contains(&k.plaintext));
    }

    #[test]
    fn generated_hash_is_the_deterministic_lookup_hash() {
        let k = generate("sk_test").unwrap();
        assert_eq!(lookup_hash(&k.plaintext), k.hash);
    }

    #[test]
    fn different_keys_have_different_lookup_hashes() {
        let k = generate("sk_test").unwrap();
        let other = generate("sk_test").unwrap();
        assert_ne!(lookup_hash(&other.plaintext), k.hash);
    }

    #[test]
    fn same_plaintext_hashes_differently_each_time() {
        // Random salt per hash — two hashes of the same key must differ, and
        // both must still verify.
        let h1 = hash("sk_test_abc").unwrap();
        let h2 = hash("sk_test_abc").unwrap();
        assert_ne!(h1, h2);
        assert!(verify("sk_test_abc", &h1).unwrap());
        assert!(verify("sk_test_abc", &h2).unwrap());
    }

    #[test]
    fn malformed_stored_hash_is_an_error_not_a_false() {
        let result = verify("sk_test_abc", "not-a-valid-hash");
        assert!(matches!(result, Err(KeyError::MalformedHash(_))));
    }

    #[test]
    fn split_prefix_handles_underscored_prefixes() {
        let (prefix, body) = split_prefix("sk_test_abc123").unwrap();
        assert_eq!(prefix, "sk_test");
        assert_eq!(body, "abc123");
    }

    #[test]
    fn split_prefix_rejects_malformed() {
        assert!(split_prefix("nokeyunderscore").is_none());
        assert!(split_prefix("sk_test_").is_none());
    }

    #[test]
    fn test_key_assertion() {
        assert!(is_test_key(&generate("sk_test").unwrap().plaintext));
        assert!(is_test_key(&generate("pk_test").unwrap().plaintext));
        assert!(!is_test_key("sk_live_abc"));
    }
}
