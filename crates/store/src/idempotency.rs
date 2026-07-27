//! Idempotency key persistence — what makes retried requests safe.
//!
//! The algorithm lives here as three functions the middleware calls in order:
//!
//!   1. `acquire`  — atomic INSERT ... ON CONFLICT DO NOTHING. Returns an
//!                   `AcquireOutcome` telling the caller whether it owns the
//!                   request, should replay a stored response, or must reject.
//!   2. `complete` — store the response so a later replay can return it.
//!   3. `purge_expired` — housekeeping job.
//!
//! The atomicity of step 1 is the whole point. `INSERT ... ON CONFLICT DO
//! NOTHING` either creates the row (we own it) or doesn't (someone else does),
//! in a single statement with no read-then-write race. Fifty concurrent
//! requests with the same key produce exactly one owner.

use domain::id::MerchantId;
use serde_json::Value as Json;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::StoreResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "idempotency_state", rename_all = "snake_case")]
pub enum IdempotencyState {
    InProgress,
    Completed,
}

/// What the caller should do after attempting to acquire a key.
#[derive(Debug, Clone, PartialEq)]
pub enum AcquireOutcome {
    /// We inserted the row — execute the request, then call `complete`.
    Acquired,
    /// Key already completed with a MATCHING request hash — replay this
    /// response verbatim instead of executing again.
    Replay { status: i32, body: Option<Json> },
    /// Key already completed but with a DIFFERENT request body. The client
    /// reused a key for a different request — a client bug. Return 409.
    Mismatch,
    /// Another request with this key is in flight right now. Return 409;
    /// do not queue, do not wait.
    InFlight,
}

/// Atomically claim the key, or discover why we can't.
pub async fn acquire(
    pool: &PgPool,
    merchant_id: MerchantId,
    key: &str,
    endpoint: &str,
    request_hash: &str,
    expires_at: OffsetDateTime,
    now: OffsetDateTime,
) -> StoreResult<AcquireOutcome> {
    // Single atomic attempt to take ownership.
    let inserted = sqlx::query!(
        r#"
        INSERT INTO idempotency_keys
            (merchant_id, key, endpoint, request_hash, state, expires_at, created_at)
        VALUES ($1, $2, $3, $4, 'in_progress', $5, $6)
        ON CONFLICT (merchant_id, key) DO NOTHING
        "#,
        merchant_id.as_uuid(),
        key,
        endpoint,
        request_hash,
        expires_at,
        now,
    )
    .execute(pool)
    .await?
    .rows_affected();

    if inserted == 1 {
        return Ok(AcquireOutcome::Acquired);
    }

    // Someone else holds it. Find out what state it's in.
    let existing = sqlx::query!(
        r#"
        SELECT request_hash, state AS "state: IdempotencyState",
               response_status, response_body
        FROM idempotency_keys
        WHERE merchant_id = $1 AND key = $2
        "#,
        merchant_id.as_uuid(),
        key,
    )
    .fetch_one(pool)
    .await?;

    match existing.state {
        IdempotencyState::InProgress => Ok(AcquireOutcome::InFlight),
        IdempotencyState::Completed => {
            if existing.request_hash == request_hash {
                Ok(AcquireOutcome::Replay {
                    status: existing.response_status.unwrap_or(200),
                    body: existing.response_body,
                })
            } else {
                Ok(AcquireOutcome::Mismatch)
            }
        }
    }
}

/// Store the response and flip to completed, so future replays return it.
pub async fn complete(
    pool: &PgPool,
    merchant_id: MerchantId,
    key: &str,
    status: i32,
    body: &Json,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE idempotency_keys
        SET state = 'completed', response_status = $3, response_body = $4
        WHERE merchant_id = $1 AND key = $2
        "#,
        merchant_id.as_uuid(),
        key,
        status,
        body,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Release a key whose request failed before completing, so the client can
/// legitimately retry rather than being stuck behind a permanent `InFlight`.
pub async fn release(pool: &PgPool, merchant_id: MerchantId, key: &str) -> StoreResult<()> {
    sqlx::query!(
        r#"
        DELETE FROM idempotency_keys
        WHERE merchant_id = $1 AND key = $2 AND state = 'in_progress'
        "#,
        merchant_id.as_uuid(),
        key,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Housekeeping: drop keys past their 24h window.
pub async fn purge_expired(pool: &PgPool, now: OffsetDateTime) -> StoreResult<u64> {
    let affected = sqlx::query!(
        r#"DELETE FROM idempotency_keys WHERE expires_at < $1"#,
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

    fn expires() -> OffsetDateTime {
        now() + time::Duration::hours(24)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn first_caller_acquires(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let out = acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        assert_eq!(out, AcquireOutcome::Acquired);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn second_caller_while_in_flight_is_rejected(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();

        let out = acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        assert_eq!(out, AcquireOutcome::InFlight);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn completed_key_replays_the_original_response(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        complete(&pool, mid, "key_1", 201, &json!({"id": "pay_1"}))
            .await
            .unwrap();

        let out = acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        match out {
            AcquireOutcome::Replay { status, body } => {
                assert_eq!(status, 201);
                assert_eq!(body.unwrap()["id"], "pay_1");
            }
            other => panic!("expected Replay, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn same_key_different_body_is_a_mismatch(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        complete(&pool, mid, "key_1", 201, &json!({})).await.unwrap();

        // Different request hash for the same key.
        let out = acquire(&pool, mid, "key_1", "/v1/payments", "hash_DIFFERENT", expires(), now())
            .await
            .unwrap();
        assert_eq!(out, AcquireOutcome::Mismatch);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn release_allows_a_legitimate_retry(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        // Request blew up before completing.
        release(&pool, mid, "key_1").await.unwrap();

        let out = acquire(&pool, mid, "key_1", "/v1/payments", "hash_a", expires(), now())
            .await
            .unwrap();
        assert_eq!(out, AcquireOutcome::Acquired);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn keys_are_scoped_per_merchant(pool: PgPool) {
        let m1 = seed_merchant(&pool).await;
        let m2 = {
            let m = domain::merchant::Merchant::new("Beta", "b@beta.dev", now()).unwrap();
            crate::merchant::insert(&pool, &m).await.unwrap();
            m.id
        };

        acquire(&pool, m1, "shared_key", "/v1/payments", "h", expires(), now())
            .await
            .unwrap();
        // Same key string, different merchant — must be independent.
        let out = acquire(&pool, m2, "shared_key", "/v1/payments", "h", expires(), now())
            .await
            .unwrap();
        assert_eq!(out, AcquireOutcome::Acquired);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn purge_removes_expired(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        acquire(&pool, mid, "old_key", "/v1/payments", "h", now(), now())
            .await
            .unwrap();

        let removed = purge_expired(&pool, now() + time::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(removed, 1);
    }
}