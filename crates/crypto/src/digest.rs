//! Non-secret hashing.
//!
//! Everything here is a plain SHA-256 — fast, deterministic, and used for
//! *identity* rather than *secrecy*: index lookups, idempotency request
//! fingerprints, log correlation tags. Anything protecting a low-entropy secret
//! belongs in `api_key` (Argon2id) instead, not here.

use sha2::{Digest, Sha256};

/// SHA-256 of arbitrary bytes, hex encoded.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_known_empty_digest() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn is_deterministic_and_collision_free_on_near_misses() {
        assert_eq!(sha256_hex(b"a"), sha256_hex(b"a"));
        assert_ne!(sha256_hex(b"a"), sha256_hex(b"b"));
        // Length matters: concatenation must not alias.
        assert_ne!(sha256_hex(b"ab"), sha256_hex(b"a\0b"));
    }

    #[test]
    fn output_is_lowercase_hex_of_fixed_width() {
        let d = sha256_hex(b"payment-sandbox");
        assert_eq!(d.len(), 64);
        assert!(d
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
}
