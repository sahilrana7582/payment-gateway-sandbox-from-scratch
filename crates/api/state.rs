//! Application state shared across handlers.

use std::sync::Arc;

use domain::clock::Clock;
use engine::{OrderService, PaymentService};
use sqlx::PgPool;

use crate::auth::AuthCache;

pub struct AppState {
    pub pool: PgPool,
    pub clock: Arc<dyn Clock>,
    pub orders: OrderService,
    pub payments: PaymentService,
    /// Short-lived memo of API key lookups, so authentication does not cost a
    /// database round trip per request. See `auth` for the revocation-window
    /// tradeoff this buys — the key-revoke handler must call
    /// `AuthCache::invalidate_key`.
    pub auth_cache: AuthCache,
    /// When true, payment creation sleeps for the simulator's latency before
    /// responding, so integrators experience realistic timing. Tests turn it
    /// off; the server turns it on.
    pub simulate_latency: bool,
}

impl AppState {
    pub fn new(
        pool: PgPool,
        clock: Arc<dyn Clock>,
        fingerprint_pepper: String,
        simulate_latency: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            orders: OrderService::new(pool.clone(), clock.clone()),
            payments: PaymentService::new(pool.clone(), clock.clone(), fingerprint_pepper),
            pool,
            clock,
            auth_cache: AuthCache::new(),
            simulate_latency,
        })
    }
}
