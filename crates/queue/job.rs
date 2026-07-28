//! The job abstraction. A `JobHandler` knows how to execute one `kind` of job;
//! the worker loop routes claimed rows to the right handler by kind string.
//!
//! Handlers live in the crates that own the work (webhook delivery lives in
//! `webhooks`, settlement in `engine`), so `queue` stays free of business logic
//! and every crate can register its own without this crate depending on them.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value as Json;
use sqlx::PgPool;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JobError {
    /// Transient — retry per the ladder (endpoint down, timeout, 5xx).
    #[error("retryable: {0}")]
    Retryable(String),

    /// Permanent — do not retry, go straight to dead (malformed payload,
    /// deleted resource). Retrying would never succeed.
    #[error("permanent: {0}")]
    Permanent(String),
}

pub type JobResult = Result<(), JobError>;

/// Boxed future, because trait methods can't be `async fn` and stay
/// object-safe. `JobHandler` must be object-safe so the worker can hold a
/// `Vec<Arc<dyn JobHandler>>` of handlers registered by different crates.
pub type BoxJobFuture<'a> = Pin<Box<dyn Future<Output = JobResult> + Send + 'a>>;

/// Implemented by each crate that owns a kind of background work.
pub trait JobHandler: Send + Sync + 'static {
    /// The `kind` string this handler claims. Must match what `enqueue` wrote.
    fn kind(&self) -> &'static str;

    /// Execute one job. The pool is passed so handlers can open their own
    /// transactions — the worker does NOT hold a transaction open while a job
    /// runs, since jobs can take seconds and holding a lock that long would
    /// block other workers.
    fn handle<'a>(&'a self, pool: &'a PgPool, payload: &'a Json) -> BoxJobFuture<'a>;
}

/// Well-known job kinds. Constants rather than an enum, because handlers are
/// registered across crates and an enum here would force `queue` to know them
/// all — which would invert the dependency graph.
pub mod kinds {
    pub const DELIVER_WEBHOOK: &str = "deliver_webhook";
    pub const EXPIRE_ORDER: &str = "expire_order";
    pub const OPEN_DISPUTE: &str = "open_dispute";
    pub const RELEASE_SETTLEMENT: &str = "release_settlement";
    pub const PURGE_EXPIRED_DATA: &str = "purge_expired_data";
}
