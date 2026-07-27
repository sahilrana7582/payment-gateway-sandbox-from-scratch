//! HMAC-SHA256 signing for the two places we prove authenticity:
//!
//!   1. **Webhook signatures** — we sign every webhook body so the merchant
//!      can verify it came from us and isn't a replay.
//!   2. **Checkout handoff signatures** — when checkout closes, the browser
//!      hands `{order_id, payment_id, signature}` back to the merchant's page.
//!      The merchant MUST verify that signature server-side before marking
//!      their order paid. Without it, anyone can call their success handler
//!      from the browser console and claim they paid. This is the single most
//!      commonly skipped step in real integrations.
//!
//! # Timestamped signature scheme
//!
//! We sign `"{timestamp}.{body}"`, not just `{body}`, and send both:
//!
//! ```text
//! X-Sandbox-Signature: t=1769270400,v1=8a7f3c...
//! ```
//!
//! The consumer rejects any timestamp outside a tolerance window (default 300s).
//! Without the timestamp, an attacker who captured one valid webhook could
//! replay it forever. This is the Stripe refinement of the scheme.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Default replay tolerance: reject signatures whose timestamp is more than
/// this many seconds from now, in either direction.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("signature header is malformed")]
    MalformedHeader,

    #[error("signature does not match")]
    Mismatch,

    #[error("timestamp outside tolerance window (drift {drift}s, max {max}s)")]
    TimestampOutOfRange { drift: i64, max: i64 },
}

/// Compute the raw HMAC-SHA256 of `{timestamp}.{payload}`, hex encoded.
pub fn compute(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

/// Build the full header value: `t={timestamp},v1={signature}`.
pub fn build_header(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let sig = compute(secret, timestamp, payload);
    format!("t={timestamp},v1={sig}")
}

/// Verify a received header against the raw body.
///
/// Two checks, both required:
///   1. The HMAC matches — proves it came from someone holding the secret.
///   2. The timestamp is within tolerance — proves it isn't a replay.
///
/// Comparison uses the `hmac` crate's `verify_slice`, which is constant-time.
/// Comparing with `==` would leak, through timing, how many leading bytes an
/// attacker guessed correctly, letting them derive a valid signature byte by
/// byte. This is the mistake to warn integrators about loudly in the docs.
pub fn verify(
    secret: &str,
    header: &str,
    payload: &[u8],
    now_unix: i64,
    tolerance_secs: i64,
) -> Result<(), SignatureError> {
    let (timestamp, provided) = parse_header(header)?;

    let drift = (now_unix - timestamp).abs();
    if drift > tolerance_secs {
        return Err(SignatureError::TimestampOutOfRange {
            drift,
            max: tolerance_secs,
        });
    }

    let provided_bytes = hex::decode(provided).map_err(|_| SignatureError::MalformedHeader)?;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);

    // Constant-time comparison. Never use `==` here.
    mac.verify_slice(&provided_bytes)
        .map_err(|_| SignatureError::Mismatch)
}

/// Parse `t=...,v1=...` into its parts.
fn parse_header(header: &str) -> Result<(i64, &str), SignatureError> {
    let mut timestamp: Option<i64> = None;
    let mut signature: Option<&str> = None;

    for part in header.split(',') {
        let (k, v) = part
            .trim()
            .split_once('=')
            .ok_or(SignatureError::MalformedHeader)?;
        match k {
            "t" => timestamp = v.parse().ok(),
            "v1" => signature = Some(v),
            _ => {} // ignore unknown versions, so we can add v2 later
        }
    }

    match (timestamp, signature) {
        (Some(t), Some(s)) => Ok((t, s)),
        _ => Err(SignatureError::MalformedHeader),
    }
}

// ============================================================================
// Checkout handoff signature
// ============================================================================

/// Sign the `{order_id}|{payment_id}` pair with the merchant's SECRET key.
///
/// Deliberately uses the API secret, not the webhook secret: the merchant
/// verifies this on their own server with a credential they already hold, with
/// no extra setup. Matches Razorpay's construction so the integration lesson
/// transfers directly.
pub fn sign_handoff(secret_key: &str, order_id: &str, payment_id: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret_key.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(order_id.as_bytes());
    mac.update(b"|");
    mac.update(payment_id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verification of a handoff signature.
pub fn verify_handoff(
    secret_key: &str,
    order_id: &str,
    payment_id: &str,
    provided: &str,
) -> Result<(), SignatureError> {
    let provided_bytes = hex::decode(provided).map_err(|_| SignatureError::MalformedHeader)?;

    let mut mac =
        HmacSha256::new_from_slice(secret_key.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(order_id.as_bytes());
    mac.update(b"|");
    mac.update(payment_id.as_bytes());

    mac.verify_slice(&provided_bytes)
        .map_err(|_| SignatureError::Mismatch)
}

/// Generate a webhook signing secret (`whsec_...`). Separate from API keys so
/// it can be rotated independently and a leak of one doesn't compromise the other.
pub fn generate_webhook_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("whsec_{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_secret";
    const BODY: &[u8] = br#"{"id":"evt_1","type":"payment.captured"}"#;
    const NOW: i64 = 1_769_270_400;

    #[test]
    fn signature_verifies_round_trip() {
        let header = build_header(SECRET, NOW, BODY);
        assert!(verify(SECRET, &header, BODY, NOW, DEFAULT_TOLERANCE_SECS).is_ok());
    }

    #[test]
    fn header_has_expected_shape() {
        let header = build_header(SECRET, NOW, BODY);
        assert!(header.starts_with("t=1769270400,v1="));
    }

    #[test]
    fn wrong_secret_fails() {
        let header = build_header(SECRET, NOW, BODY);
        let err = verify("wrong_secret", &header, BODY, NOW, DEFAULT_TOLERANCE_SECS).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn tampered_body_fails() {
        let header = build_header(SECRET, NOW, BODY);
        let tampered = br#"{"id":"evt_1","type":"payment.failed"}"#;
        let err = verify(SECRET, &header, tampered, NOW, DEFAULT_TOLERANCE_SECS).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn replay_outside_tolerance_is_rejected() {
        let header = build_header(SECRET, NOW, BODY);
        // Same valid signature, replayed 10 minutes later.
        let later = NOW + 600;
        let err = verify(SECRET, &header, BODY, later, DEFAULT_TOLERANCE_SECS).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampOutOfRange { .. }));
    }

    #[test]
    fn within_tolerance_is_accepted() {
        let header = build_header(SECRET, NOW, BODY);
        // 4 minutes later — inside the 5 minute window.
        assert!(verify(SECRET, &header, BODY, NOW + 240, DEFAULT_TOLERANCE_SECS).is_ok());
    }

    #[test]
    fn future_timestamps_also_bounded() {
        // Clock skew in either direction is bounded, not just the past.
        let header = build_header(SECRET, NOW + 600, BODY);
        let err = verify(SECRET, &header, BODY, NOW, DEFAULT_TOLERANCE_SECS).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampOutOfRange { .. }));
    }

    #[test]
    fn malformed_headers_rejected() {
        for bad in ["", "garbage", "t=abc,v1=def", "v1=onlysig", "t=123"] {
            assert_eq!(
                verify(SECRET, bad, BODY, NOW, DEFAULT_TOLERANCE_SECS).unwrap_err(),
                SignatureError::MalformedHeader
            );
        }
    }

    #[test]
    fn unknown_signature_versions_are_ignored_not_fatal() {
        // Forward compatibility: a v2= we don't understand must not break v1.
        let sig = compute(SECRET, NOW, BODY);
        let header = format!("t={NOW},v1={sig},v2=futurescheme");
        assert!(verify(SECRET, &header, BODY, NOW, DEFAULT_TOLERANCE_SECS).is_ok());
    }

    #[test]
    fn handoff_signature_round_trip() {
        let sig = sign_handoff("sk_test_abc", "order_1", "pay_1");
        assert!(verify_handoff("sk_test_abc", "order_1", "pay_1", &sig).is_ok());
    }

    #[test]
    fn handoff_rejects_swapped_ids() {
        // Proves the separator matters: signing (a,b) must not validate (b,a).
        let sig = sign_handoff("sk_test_abc", "order_1", "pay_1");
        assert!(verify_handoff("sk_test_abc", "pay_1", "order_1", &sig).is_err());
    }

    #[test]
    fn handoff_rejects_forged_payment_id() {
        // The attack the handoff signature exists to prevent: a customer
        // calling the merchant's success handler with an id they made up.
        let sig = sign_handoff("sk_test_abc", "order_1", "pay_1");
        assert!(verify_handoff("sk_test_abc", "order_1", "pay_FORGED", &sig).is_err());
    }

    #[test]
    fn webhook_secrets_are_unique_and_prefixed() {
        let a = generate_webhook_secret();
        let b = generate_webhook_secret();
        assert!(a.starts_with("whsec_"));
        assert_ne!(a, b);
    }
}
