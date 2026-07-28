//! Operational limits for the HTTP surface.
//!
//! Every number a request can push against lives here rather than being spelled
//! inline at the one call site that happens to enforce it. That matters for two
//! reasons: an operator tuning a deployment has exactly one file to read, and a
//! test can shrink a limit to prove the enforcement path works without having to
//! actually send a megabyte.
//!
//! These are **defence in depth**, not business rules. Business rules (minimum
//! order amount, allowed currencies) belong in `engine`, where they apply no
//! matter which transport invoked them.

use std::time::Duration;

/// Tunables for the API layer. [`Default`] is the production shape; tests
/// override the fields they care about.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Largest request body we will buffer, in bytes. A payments API deals in
    /// small JSON documents; anything past this is either a mistake or an
    /// attempt to make us allocate. 256 KiB leaves enormous headroom over the
    /// ~500-byte payment request while staying cheap to hold per connection.
    pub max_body_bytes: usize,

    /// Largest response we will buffer in order to record it against an
    /// idempotency key. A response bigger than this is still returned to the
    /// caller — it just is not replayable, and says so in the log.
    pub max_recorded_response_bytes: usize,

    /// Wall-clock ceiling on a single request. Past this the caller gets a 504
    /// and the handler future is dropped, releasing its database connection.
    /// Without it, one wedged query holds a pool slot until the client gives up.
    pub request_timeout: Duration,

    /// How long an idempotency key remains replayable. 24h matches the industry
    /// norm and the `idempotency_keys` purge job.
    pub idempotency_ttl: Duration,

    /// Page size used when the caller does not ask for one.
    pub default_page_size: i64,

    /// Largest page a caller may request. Requests above this are rejected with
    /// a message naming the maximum, rather than silently clamped — a caller
    /// paginating on `limit=500` and receiving 100 items with no explanation
    /// will conclude they have reached the end of the collection.
    pub max_page_size: i64,

    /// When true, payment creation sleeps for the simulator's decided latency
    /// before responding, so integrators experience realistic timing. The
    /// server turns this on; tests turn it off.
    pub simulate_latency: bool,

    /// Ceiling on that artificial sleep. The simulator is trusted code, but a
    /// latency budget that can only be violated by editing this repository is
    /// still worth stating — it keeps "realistic timing" from ever becoming
    /// "connection held open indefinitely".
    pub max_simulated_latency: Duration,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 256 * 1024,
            max_recorded_response_bytes: 256 * 1024,
            request_timeout: Duration::from_secs(30),
            idempotency_ttl: Duration::from_secs(24 * 60 * 60),
            default_page_size: 10,
            max_page_size: 100,
            simulate_latency: true,
            max_simulated_latency: Duration::from_secs(5),
        }
    }
}

impl ApiConfig {
    /// The configuration tests want: every limit still enforced, but nothing
    /// that makes a test suite wait.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            simulate_latency: false,
            ..Self::default()
        }
    }

    /// [`idempotency_ttl`](Self::idempotency_ttl) as the `time` crate's duration,
    /// which is what the store layer speaks.
    #[must_use]
    pub fn idempotency_ttl_time(&self) -> time::Duration {
        time::Duration::try_from(self.idempotency_ttl).unwrap_or_else(|_| time::Duration::hours(24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_internally_consistent() {
        let c = ApiConfig::default();
        assert!(c.default_page_size <= c.max_page_size);
        assert!(c.max_simulated_latency < c.request_timeout);
        assert!(c.max_body_bytes > 0);
    }

    #[test]
    fn test_profile_only_disables_the_artificial_sleep() {
        let (t, d) = (ApiConfig::for_tests(), ApiConfig::default());
        assert!(!t.simulate_latency);
        assert_eq!(t.max_body_bytes, d.max_body_bytes);
        assert_eq!(t.request_timeout, d.request_timeout);
    }

    #[test]
    fn idempotency_ttl_converts_without_loss() {
        assert_eq!(
            ApiConfig::default().idempotency_ttl_time(),
            time::Duration::hours(24)
        );
    }
}
