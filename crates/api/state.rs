//! Application state shared across handlers.
//!
//! One `Arc<AppState>` per process, cloned into every request. It holds the
//! things that are expensive to build (the pool, the services) and the things
//! that must be shared to be correct (the auth cache — a cache per request would
//! cache nothing).
//!
//! What it deliberately does not hold is anything request-scoped. The
//! authenticated caller lives in request extensions and the request id in a
//! task-local, because a field on shared state that means "the current request"
//! is a data race waiting for its first concurrent user.

use std::sync::Arc;

use domain::clock::Clock;
use engine::refund::RefundService;
use engine::{OrderService, PaymentService};
use sqlx::PgPool;

use crate::auth::AuthCache;
use crate::config::ApiConfig;

pub struct AppState {
    pub pool: PgPool,
    pub clock: Arc<dyn Clock>,
    pub orders: OrderService,
    pub payments: PaymentService,
    pub refunds: RefundService,
    /// Short-lived memo of API key lookups, so authentication does not cost a
    /// database round trip per request. See `auth` for the revocation-window
    /// tradeoff this buys — the key-revoke handler must call
    /// `AuthCache::invalidate_key`.
    pub auth_cache: AuthCache,
    /// Operational limits. See [`ApiConfig`] for what each one defends against.
    pub config: ApiConfig,
}

impl AppState {
    /// The common case: production defaults, with the one knob the server
    /// actually varies.
    pub fn new(
        pool: PgPool,
        clock: Arc<dyn Clock>,
        fingerprint_pepper: String,
        simulate_latency: bool,
    ) -> Arc<Self> {
        Self::with_config(
            pool,
            clock,
            fingerprint_pepper,
            ApiConfig {
                simulate_latency,
                ..ApiConfig::default()
            },
        )
    }

    /// Full control over the limits, for tests that need to prove an enforcement
    /// path fires without generating the traffic to trigger it naturally.
    pub fn with_config(
        pool: PgPool,
        clock: Arc<dyn Clock>,
        fingerprint_pepper: String,
        config: ApiConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            orders: OrderService::new(pool.clone(), clock.clone()),
            payments: PaymentService::new(pool.clone(), clock.clone(), fingerprint_pepper),
            refunds: RefundService::new(pool.clone(), clock.clone()),
            pool,
            clock,
            auth_cache: AuthCache::new(),
            config,
        })
    }

    /// State for tests of middleware that never touches the database.
    ///
    /// The pool is built lazily and is never connected — any test that reaches
    /// for it will fail loudly at query time rather than quietly reading from
    /// somewhere unexpected.
    #[cfg(test)]
    pub(crate) fn for_timeout_tests(config: ApiConfig) -> Arc<Self> {
        use domain::clock::FixedClock;
        use time::macros::datetime;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("a lazy pool never connects, so this cannot fail");

        Self::with_config(
            pool,
            Arc::new(FixedClock::new(datetime!(2026-01-01 00:00:00 UTC))),
            "test_pepper".into(),
            config,
        )
    }
}
