//! Card fingerprinting.
//!
//! A fingerprint lets us answer "is this the same card as before?" — for
//! duplicate detection, velocity checks, and linking a customer's repeat
//! payments — WITHOUT retaining the card number.
//!
//! # Why HMAC and not a plain hash
//!
//! A bare SHA-256 of a card number is reversible in practice: the space of
//! valid card numbers is small enough (a few trillion, heavily constrained by
//! IIN ranges and the Luhn check) to brute-force a rainbow table. Keying the
//! hash with a server-side secret pepper makes that infeasible — an attacker
//! with a database dump but not the pepper cannot recover any PAN.
//!
//! The pepper is a server secret, NOT stored per-row. It comes from config and
//! is never written to the database, so a database compromise alone is not
//! enough to attack the fingerprints.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Compute the fingerprint of a card number.
///
/// Input is normalized (non-digits stripped) so `"4242 4242 4242 4242"` and
/// `"4242424242424242"` produce the same fingerprint — otherwise formatting
/// differences would defeat duplicate detection.
pub fn card_fingerprint(pepper: &str, card_number: &str) -> String {
    let digits: String = card_number.chars().filter(|c| c.is_ascii_digit()).collect();

    let mut mac =
        HmacSha256::new_from_slice(pepper.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(digits.as_bytes());

    // Truncate to 32 hex chars (128 bits). Ample against collisions, and a
    // shorter value keeps rows and logs tidy.
    hex::encode(mac.finalize().into_bytes())[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEPPER: &str = "test_pepper_do_not_use_in_prod";

    #[test]
    fn same_card_same_fingerprint() {
        let a = card_fingerprint(PEPPER, "4242424242424242");
        let b = card_fingerprint(PEPPER, "4242424242424242");
        assert_eq!(a, b);
    }

    #[test]
    fn formatting_is_normalized() {
        let spaced = card_fingerprint(PEPPER, "4242 4242 4242 4242");
        let dashed = card_fingerprint(PEPPER, "4242-4242-4242-4242");
        let plain = card_fingerprint(PEPPER, "4242424242424242");
        assert_eq!(spaced, plain);
        assert_eq!(dashed, plain);
    }

    #[test]
    fn different_cards_differ() {
        let a = card_fingerprint(PEPPER, "4242424242424242");
        let b = card_fingerprint(PEPPER, "5555555555554444");
        assert_ne!(a, b);
    }

    #[test]
    fn different_pepper_changes_everything() {
        // Rotating the pepper invalidates all existing fingerprints — that's
        // expected and documented; it's a re-fingerprinting migration.
        let a = card_fingerprint(PEPPER, "4242424242424242");
        let b = card_fingerprint("different_pepper", "4242424242424242");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_does_not_contain_the_pan() {
        let fp = card_fingerprint(PEPPER, "4242424242424242");
        assert!(!fp.contains("4242"));
        assert!(!fp.contains("424242424242"));
    }

    #[test]
    fn fingerprint_length_is_stable() {
        assert_eq!(card_fingerprint(PEPPER, "4242424242424242").len(), 32);
        assert_eq!(card_fingerprint(PEPPER, "378282246310005").len(), 32); // amex, 15 digits
    }
}
