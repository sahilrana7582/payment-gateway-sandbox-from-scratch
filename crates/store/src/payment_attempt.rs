//! Payment attempt persistence — one row per confirmation attempt, recording
//! the simulator's verdict and latency for the dashboard timeline.

use domain::id::PaymentId;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;
use crate::tx::PgTx;

pub async fn insert(
    tx: &mut PgTx<'_>,
    payment_id: PaymentId,
    attempt_number: i32,
    outcome: &str,
    decline_code: Option<&str>,
    latency_ms: i32,
    now: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO payment_attempts
            (id, payment_id, attempt_number, outcome, decline_code,
             network_response, latency_ms, created_at)
        VALUES ($1, $2, $3, $4, $5, '{}', $6, $7)
        "#,
        Uuid::now_v7(),
        payment_id.as_uuid(),
        attempt_number,
        outcome,
        decline_code,
        latency_ms,
        now,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
