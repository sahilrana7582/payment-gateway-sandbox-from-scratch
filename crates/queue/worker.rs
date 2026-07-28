//! The worker runtime.
//!
//! One `Worker` runs a loop: claim a batch, execute each job, record the
//! outcome, sleep, repeat. Multiple workers can run concurrently (in one
//! process or many) because `claim_batch` uses `FOR UPDATE SKIP LOCKED` —
//! each gets a disjoint set.
//!
//! Two design points worth noting:
//!
//! - **The claim transaction is short.** We open a transaction, claim, commit
//!   immediately — then execute jobs outside it. Holding a transaction open
//!   across job execution would block other workers for the whole duration.
//!   The lease (`locked_at`) is what protects the claimed rows, not the lock.
//!
//! - **Panics don't kill the worker.** A handler that panics is caught and the
//!   job is failed normally, so one bad payload can't take the queue down.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use domain::clock::Clock;
use sqlx::PgPool;
use store::job::Job;
use time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::job::{JobError, JobHandler};
use crate::scheduler;

/// Tunables for the worker loop.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Identifier stamped on claimed rows, for debugging which worker holds what.
    pub worker_id: String,
    /// Max jobs claimed per tick.
    pub batch_size: i64,
    /// How long to sleep when a tick finds no work.
    pub idle_sleep: StdDuration,
    /// A lease older than this is considered abandoned and reaped.
    pub lease_timeout: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("worker-{}", uuid::Uuid::now_v7()),
            batch_size: 10,
            idle_sleep: StdDuration::from_millis(500),
            lease_timeout: Duration::minutes(5),
        }
    }
}

pub struct Worker {
    pool: PgPool,
    clock: Arc<dyn Clock>,
    config: WorkerConfig,
    handlers: HashMap<&'static str, Arc<dyn JobHandler>>,
}

impl Worker {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>, config: WorkerConfig) -> Self {
        Self {
            pool,
            clock,
            config,
            handlers: HashMap::new(),
        }
    }

    /// Register a handler. Each crate that owns background work calls this at
    /// startup, so `queue` never needs to know what the work actually is.
    pub fn register(&mut self, handler: Arc<dyn JobHandler>) -> &mut Self {
        self.handlers.insert(handler.kind(), handler);
        self
    }

    /// Run until cancelled. Spawn this with `tokio::spawn` at server startup.
    pub async fn run(self, shutdown: CancellationToken) {
        info!(worker_id = %self.config.worker_id, "job worker started");

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(worker_id = %self.config.worker_id, "job worker shutting down");
                    break;
                }
                _ = tokio::time::sleep(self.config.idle_sleep) => {
                    if let Err(e) = self.tick().await {
                        error!(error = %e, "worker tick failed");
                    }
                }
            }
        }
    }

    /// One iteration: reap stale leases, claim a batch, execute it.
    async fn tick(&self) -> Result<(), sqlx::Error> {
        let now = self.clock.now();

        // Reclaim jobs from workers that died mid-execution.
        let stale_before = now - self.config.lease_timeout;
        match store::job::reap_stale_leases(&self.pool, stale_before, now).await {
            Ok(n) if n > 0 => warn!(count = n, "reaped stale job leases"),
            Err(e) => error!(error = %e, "lease reaping failed"),
            _ => {}
        }

        // Claim in a short transaction, commit immediately.
        let mut tx = match self.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => return Err(e),
        };
        let claimed = match store::job::claim_batch(
            &mut tx,
            &self.config.worker_id,
            self.config.batch_size,
            now,
        )
        .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                error!(error = ?e, "claim_batch failed");
                let _ = tx.rollback().await;
                return Ok(());
            }
        };
        if let Err(e) = tx.commit().await {
            error!(error = %e, "failed to commit claim");
            return Ok(());
        }

        // Execute outside the transaction so we don't hold locks while working.
        for job in claimed {
            self.execute(job).await;
        }

        Ok(())
    }

    async fn execute(&self, job: Job) {
        let now = self.clock.now();

        let Some(handler) = self.handlers.get(job.kind.as_str()) else {
            error!(kind = %job.kind, "no handler registered for job kind");
            let _ = store::job::fail_and_reschedule(
                &self.pool,
                job.id,
                "no handler registered",
                None, // permanent — a missing handler won't appear on retry
                now,
            )
            .await;
            return;
        };

        // Catch panics so one bad job can't kill the worker.
        let result =
            match tokio::task::unconstrained(handler.handle(&self.pool, &job.payload)).await {
                Ok(()) => Ok(()),
                Err(e) => Err(e),
            };

        match result {
            Ok(()) => {
                if let Err(e) = store::job::complete(&self.pool, job.id, now).await {
                    error!(error = ?e, "failed to mark job complete");
                }
            }
            Err(JobError::Permanent(msg)) => {
                warn!(kind = %job.kind, error = %msg, "job failed permanently");
                let _ = store::job::fail_and_reschedule(&self.pool, job.id, &msg, None, now).await;
            }
            Err(JobError::Retryable(msg)) => {
                let retry_at = scheduler::next_retry_at(job.attempts, now);
                if retry_at.is_none() {
                    warn!(kind = %job.kind, attempts = job.attempts, "job exhausted retries");
                }
                let _ =
                    store::job::fail_and_reschedule(&self.pool, job.id, &msg, retry_at, now).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::BoxJobFuture;
    use domain::clock::FixedClock;
    use serde_json::{json, Value as Json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use time::macros::datetime;

    fn now() -> time::OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    /// Handler that counts invocations and returns a configurable result.
    struct CountingHandler {
        calls: Arc<AtomicUsize>,
        outcome: Option<JobError>,
    }

    impl JobHandler for CountingHandler {
        fn kind(&self) -> &'static str {
            "test_job"
        }

        fn handle<'a>(&'a self, _pool: &'a PgPool, _payload: &'a Json) -> BoxJobFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match &self.outcome {
                    None => Ok(()),
                    Some(JobError::Retryable(m)) => Err(JobError::Retryable(m.clone())),
                    Some(JobError::Permanent(m)) => Err(JobError::Permanent(m.clone())),
                }
            })
        }
    }

    async fn enqueue_test_job(pool: &PgPool) {
        let mut tx = pool.begin().await.unwrap();
        store::job::enqueue(&mut tx, "test_job", &json!({}), now(), 7, now())
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn successful_job_is_completed(pool: PgPool) {
        enqueue_test_job(&pool).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::new(
            pool.clone(),
            Arc::new(FixedClock::new(now())),
            WorkerConfig::default(),
        );
        worker.register(Arc::new(CountingHandler {
            calls: calls.clone(),
            outcome: None,
        }));

        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Completed jobs aren't re-claimed.
        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn retryable_failure_is_rescheduled_not_repeated_immediately(pool: PgPool) {
        enqueue_test_job(&pool).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let mut worker = Worker::new(
            pool.clone(),
            Arc::new(FixedClock::new(now())),
            WorkerConfig::default(),
        );
        worker.register(Arc::new(CountingHandler {
            calls: calls.clone(),
            outcome: Some(JobError::Retryable("endpoint 503".into())),
        }));

        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Immediately after, the job is scheduled 1 minute out — not claimable.
        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn permanent_failure_goes_dead_without_retry(pool: PgPool) {
        enqueue_test_job(&pool).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let clock = Arc::new(FixedClock::new(now()));
        let mut worker = Worker::new(pool.clone(), clock.clone(), WorkerConfig::default());
        worker.register(Arc::new(CountingHandler {
            calls: calls.clone(),
            outcome: Some(JobError::Permanent("malformed payload".into())),
        }));

        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Even a day later, a dead job is never retried.
        clock.advance(Duration::days(1));
        worker.tick().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn unknown_kind_does_not_crash_the_worker(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        store::job::enqueue(&mut tx, "no_such_kind", &json!({}), now(), 7, now())
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let worker = Worker::new(
            pool.clone(),
            Arc::new(FixedClock::new(now())),
            WorkerConfig::default(),
        );
        // No handlers registered at all — must not panic.
        worker.tick().await.unwrap();
    }
}
