//! Application state shared across handlers.

use std::sync::Arc;

use domain::clock::Clock;
use engine::{OrderService, PaymentService};
use sqlx::PgPool;

pub struct AppState {
    pub pool: PgPool,
    pub clock: Arc<dyn Clock>,
    pub orders: OrderService,
    pub payments: PaymentService,
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
            simulate_latency,
        })
    }
}
