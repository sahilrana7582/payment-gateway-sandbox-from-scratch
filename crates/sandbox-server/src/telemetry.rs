//! Tracing setup.
//!
//! Plain fmt output with an env filter for the skeleton. Two upgrades land in
//! the hardening pass, both already designed for:
//!   - JSON output for structured log shipping
//!   - a custom redaction `Layer` scrubbing any field matching
//!     `secret|token|key|pan|cvc` — defense in depth on top of the design rule
//!     that card numbers never reach a log statement in the first place.

pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,sandbox_server=debug,tower_http=info".into());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}
