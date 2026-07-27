//! Job queue persistence. Postgres IS the queue.
//!
//! Two mechanisms make this work:
//!
//!   1. `enqueue` takes `&mut PgTx`. A job is inserted in the SAME transaction
//!      as the state change that needs it — a webhook-delivery job enqueued
//!      alongside a payment capture. If that transaction rolls back, the job
//!      vanishes with it. This is the transactional outbox pattern, correct by
//!      construction rather than by a background sync process.
//!
//!   2. `claim_batch` uses `FOR UPDATE SKIP LOCKED`. Multiple worker processes
//!      can call it concurrently: each locks and claims a DISJOINT set of rows,
//!      skipping any row another worker already holds, so they never block each
//!      other and never process the same job twice.

use domain::id::JobId;
use serde_json::Value as Json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;
use crate::tx::PgTx;

/// Mirrors the `job_state` Postgres enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "job_state", rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Completed,
    Failed,
    Dead,
}

/// A job as read from the queue. `kind` + `payload` are interpreted by the
/// worker that handles that kind (e.g. the webhooks crate handles
/// "deliver_webhook").
#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub kind: String,
    pub payload: Json,
    pub state: JobState,
    pub run_at: OffsetDateTime,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
}

struct JobRow {
    id: Uuid,
    kind: String,
    payload: Json,
    state: JobState,
    run_at: OffsetDateTime,
    attempts: i32,
    max_attempts: i32,
    last_error: Option<String>,
}

impl From<JobRow> for Job {
    fn from(r: JobRow) -> Self {
        Job {
            id: JobId::from_uuid(r.id),
            kind: r.kind,
            payload: r.payload,
            state: r.state,
            run_at: r.run_at,
            attempts: r.attempts,
            max_attempts: r.max_attempts,
            last_error: r.last_error,
        }
    }
}

/// Enqueue a job inside the caller's transaction. `run_at` in the future
/// schedules delayed work (a webhook retry, a T+2 settlement); pass `now` to
/// run as soon as a worker picks it up.
pub async fn enqueue(
    tx: &mut PgTx<'_>,
    kind: &str,
    payload: &Json,
    run_at: OffsetDateTime,
    max_attempts: i32,
    now: OffsetDateTime,
) -> StoreResult<JobId> {
    let id = JobId::new();

    sqlx::query!(
        r#"
        INSERT INTO jobs
            (id, kind, payload, state, run_at, attempts, max_attempts,
             created_at, updated_at)
        VALUES ($1, $2, $3, 'pending', $4, 0, $5, $6, $6)
        "#,
        id.as_uuid(),
        kind,
        payload,
        run_at,
        max_attempts,
        now,
    )
    .execute(&mut **tx)
    .await?;

    Ok(id)
}

/// Claim up to `limit` due jobs for this worker, atomically.
///
/// The query selects pending jobs whose `run_at <= now`, oldest first, locking
/// them with `FOR UPDATE SKIP LOCKED` so concurrent workers get disjoint sets.
/// The claimed rows are flipped to `running` and stamped with the lease
/// (`locked_at`, `locked_by`) in the same statement, so a crashed worker's jobs
/// can later be reclaimed by the reaper.
///
/// Runs in its own transaction (`&mut PgTx`) that the caller commits right
/// after — the lock is held only for the instant of claiming, not while the
/// jobs execute.
pub async fn claim_batch(
    tx: &mut PgTx<'_>,
    worker_id: &str,
    limit: i64,
    now: OffsetDateTime,
) -> StoreResult<Vec<Job>> {
    let rows = sqlx::query_as!(
        JobRow,
        r#"
        UPDATE jobs
        SET state = 'running',
            locked_at = $3,
            locked_by = $1,
            attempts = attempts + 1,
            updated_at = $3
        WHERE id IN (
            SELECT id FROM jobs
            WHERE state = 'pending' AND run_at <= $3
            ORDER BY run_at
            FOR UPDATE SKIP LOCKED
            LIMIT $2
        )
        RETURNING id, kind, payload, state AS "state: JobState",
                  run_at, attempts, max_attempts, last_error
        "#,
        worker_id,
        limit,
        now,
    )
    .fetch_all(&mut **tx)
    .await?;

    Ok(rows.into_iter().map(Job::from).collect())
}

/// Mark a claimed job finished successfully.
pub async fn complete(pool: &PgPool, id: JobId, now: OffsetDateTime) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE jobs
        SET state = 'completed', locked_at = NULL, locked_by = NULL, updated_at = $2
        WHERE id = $1
        "#,
        id.as_uuid(),
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed attempt. If attempts remain, reschedule to `retry_at` and
/// return to `pending`; if exhausted, move to `dead`. The retry schedule itself
/// lives in the `queue` crate's scheduler — this just persists the decision.
pub async fn fail_and_reschedule(
    pool: &PgPool,
    id: JobId,
    error: &str,
    retry_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> StoreResult<()> {
    match retry_at {
        Some(when) => {
            sqlx::query!(
                r#"
                UPDATE jobs
                SET state = 'pending', run_at = $3, last_error = $2,
                    locked_at = NULL, locked_by = NULL, updated_at = $4
                WHERE id = $1
                "#,
                id.as_uuid(),
                error,
                when,
                now,
            )
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query!(
                r#"
                UPDATE jobs
                SET state = 'dead', last_error = $2,
                    locked_at = NULL, locked_by = NULL, updated_at = $3
                WHERE id = $1
                "#,
                id.as_uuid(),
                error,
                now,
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Reaper: release jobs whose lease has expired (a worker claimed them, stamped
/// `locked_at`, then crashed before finishing). Any `running` job older than
/// `stale_before` goes back to `pending` so another worker can pick it up.
/// Returns how many were released.
pub async fn reap_stale_leases(
    pool: &PgPool,
    stale_before: OffsetDateTime,
    now: OffsetDateTime,
) -> StoreResult<u64> {
    let affected = sqlx::query!(
        r#"
        UPDATE jobs
        SET state = 'pending', locked_at = NULL, locked_by = NULL, updated_at = $2
        WHERE state = 'running' AND locked_at < $1
        "#,
        stale_before,
        now,
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn enqueue_one(pool: &PgPool, kind: &str, run_at: OffsetDateTime) -> JobId {
        let mut tx = crate::tx::begin(pool).await.unwrap();
        let id = enqueue(&mut tx, kind, &json!({"k": "v"}), run_at, 6, now())
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();
        id
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn enqueue_then_claim(pool: PgPool) {
        enqueue_one(&pool, "deliver_webhook", now()).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let claimed = claim_batch(&mut tx, "worker-1", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].kind, "deliver_webhook");
        assert_eq!(claimed[0].attempts, 1); // claim increments attempts
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn future_jobs_are_not_claimed_yet(pool: PgPool) {
        // Scheduled an hour out.
        enqueue_one(
            &pool,
            "release_settlement",
            now() + time::Duration::hours(1),
        )
        .await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let claimed = claim_batch(&mut tx, "worker-1", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(claimed.len(), 0); // not due yet
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn two_workers_get_disjoint_jobs(pool: PgPool) {
        // Enqueue 4 due jobs.
        for _ in 0..4 {
            enqueue_one(&pool, "deliver_webhook", now()).await;
        }

        // Worker 1 claims 2 in an open transaction (holds the locks).
        let mut tx1 = crate::tx::begin(&pool).await.unwrap();
        let batch1 = claim_batch(&mut tx1, "worker-1", 2, now()).await.unwrap();
        assert_eq!(batch1.len(), 2);

        // Worker 2, concurrently, claims from the remainder — SKIP LOCKED means
        // it does NOT block on worker 1's rows, it gets the other 2.
        let mut tx2 = crate::tx::begin(&pool).await.unwrap();
        let batch2 = claim_batch(&mut tx2, "worker-2", 10, now()).await.unwrap();
        assert_eq!(batch2.len(), 2);

        crate::tx::commit(tx1).await.unwrap();
        crate::tx::commit(tx2).await.unwrap();

        // No overlap.
        let ids1: Vec<_> = batch1.iter().map(|j| j.id).collect();
        for j in &batch2 {
            assert!(!ids1.contains(&j.id));
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn complete_marks_done(pool: PgPool) {
        let id = enqueue_one(&pool, "deliver_webhook", now()).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        claim_batch(&mut tx, "w", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        complete(&pool, id, now()).await.unwrap();

        // A completed job is not re-claimable.
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let again = claim_batch(&mut tx, "w", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();
        assert_eq!(again.len(), 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn fail_reschedules_when_retry_remains(pool: PgPool) {
        let id = enqueue_one(&pool, "deliver_webhook", now()).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        claim_batch(&mut tx, "w", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let retry_at = now() + time::Duration::minutes(5);
        fail_and_reschedule(&pool, id, "endpoint 500", Some(retry_at), now())
            .await
            .unwrap();

        // Not claimable now (rescheduled to +5m)...
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let soon = claim_batch(&mut tx, "w", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();
        assert_eq!(soon.len(), 0);

        // ...but claimable at retry time.
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let later = claim_batch(&mut tx, "w", 10, retry_at).await.unwrap();
        crate::tx::commit(tx).await.unwrap();
        assert_eq!(later.len(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn fail_with_no_retry_goes_dead(pool: PgPool) {
        let id = enqueue_one(&pool, "deliver_webhook", now()).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        claim_batch(&mut tx, "w", 10, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        fail_and_reschedule(&pool, id, "exhausted", None, now())
            .await
            .unwrap();

        // Dead jobs are never claimed again.
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let again = claim_batch(&mut tx, "w", 10, now() + time::Duration::days(1))
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();
        assert_eq!(again.len(), 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn reaper_releases_a_stale_lease(pool: PgPool) {
        enqueue_one(&pool, "deliver_webhook", now()).await;

        // Claim it, then commit (job is 'running', locked_at = now).
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        claim_batch(&mut tx, "crashed-worker", 10, now())
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();

        // Simulate the worker having crashed: reap leases older than 30m from
        // a point in time an hour later.
        let later = now() + time::Duration::hours(1);
        let stale_before = later - time::Duration::minutes(30);
        let released = reap_stale_leases(&pool, stale_before, later).await.unwrap();
        assert_eq!(released, 1);

        // Now re-claimable.
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let reclaimed = claim_batch(&mut tx, "worker-2", 10, later).await.unwrap();
        crate::tx::commit(tx).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
    }
}
