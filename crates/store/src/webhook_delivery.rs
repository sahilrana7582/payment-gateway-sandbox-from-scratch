//! Webhook delivery persistence. One row per (event, endpoint, attempt) — the
//! delivery log the dashboard renders with response codes and a resend button.
//! The worker updates a row's status/response after each attempt and sets
//! `next_retry_at` for the retry ladder.

use domain::id::{EventId, WebhookEndpointId};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;
use crate::tx::PgTx;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "webhook_delivery_status", rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
    Dead,
}

#[derive(Debug, Clone)]
pub struct WebhookDelivery {
    pub id: Uuid,
    pub event_id: EventId,
    pub endpoint_id: WebhookEndpointId,
    pub attempt: i32,
    pub status: DeliveryStatus,
    pub response_status: Option<i32>,
    pub response_body: Option<String>,
    pub duration_ms: Option<i32>,
    pub next_retry_at: Option<OffsetDateTime>,
}

struct DeliveryRow {
    id: Uuid,
    event_id: Uuid,
    endpoint_id: Uuid,
    attempt: i32,
    status: DeliveryStatus,
    response_status: Option<i32>,
    response_body: Option<String>,
    duration_ms: Option<i32>,
    next_retry_at: Option<OffsetDateTime>,
}

impl From<DeliveryRow> for WebhookDelivery {
    fn from(r: DeliveryRow) -> Self {
        WebhookDelivery {
            id: r.id,
            event_id: EventId::from_uuid(r.event_id),
            endpoint_id: WebhookEndpointId::from_uuid(r.endpoint_id),
            attempt: r.attempt,
            status: r.status,
            response_status: r.response_status,
            response_body: r.response_body,
            duration_ms: r.duration_ms,
            next_retry_at: r.next_retry_at,
        }
    }
}

/// Create the initial pending delivery row. Called inside the same transaction
/// that emits the event + enqueues the delivery job, so the log entry exists
/// the moment the job does.
pub async fn insert_pending(
    tx: &mut PgTx<'_>,
    event_id: EventId,
    endpoint_id: WebhookEndpointId,
    now: OffsetDateTime,
) -> StoreResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"
        INSERT INTO webhook_deliveries
            (id, event_id, endpoint_id, attempt, status, next_retry_at,
             created_at, updated_at)
        VALUES ($1, $2, $3, 1, 'pending', $4, $4, $4)
        "#,
        id,
        event_id.as_uuid(),
        endpoint_id.as_uuid(),
        now,
    )
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// Record the outcome of a delivery attempt. On success pass `Delivered` and
/// clear the retry; on a retryable failure pass `Failed` with the next time;
/// on exhaustion pass `Dead`.
#[allow(clippy::too_many_arguments)]
pub async fn record_attempt(
    pool: &PgPool,
    id: Uuid,
    status: DeliveryStatus,
    response_status: Option<i32>,
    response_body: Option<&str>,
    duration_ms: i32,
    next_retry_at: Option<OffsetDateTime>,
    attempt: i32,
    now: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE webhook_deliveries
        SET status = $2, response_status = $3, response_body = $4,
            duration_ms = $5, next_retry_at = $6, attempt = $7, updated_at = $8
        WHERE id = $1
        "#,
        id,
        status as DeliveryStatus,
        response_status,
        response_body,
        duration_ms,
        next_retry_at,
        attempt,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The delivery history for an event (dashboard view).
pub async fn list_for_event(pool: &PgPool, event_id: EventId) -> StoreResult<Vec<WebhookDelivery>> {
    let rows = sqlx::query_as!(
        DeliveryRow,
        r#"
        SELECT id, event_id, endpoint_id, attempt,
               status AS "status: DeliveryStatus",
               response_status, response_body, duration_ms, next_retry_at
        FROM webhook_deliveries
        WHERE event_id = $1
        ORDER BY attempt
        "#,
        event_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(WebhookDelivery::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed(pool: &PgPool) -> (EventId, WebhookEndpointId) {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();

        let mut tx = crate::tx::begin(pool).await.unwrap();
        let eid = crate::event::insert(&mut tx, m.id, "payment.captured", &json!({}), "v1", now())
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let ep = crate::webhook_endpoint::WebhookEndpoint {
            id: WebhookEndpointId::new(),
            merchant_id: m.id,
            url: "https://x.dev/wh".into(),
            signing_secret: "whsec".into(),
            enabled_events: vec![],
            disabled_at: None,
            created_at: now(),
        };
        crate::webhook_endpoint::insert(pool, &ep).await.unwrap();
        (eid, ep.id)
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_and_record_success(pool: PgPool) {
        let (eid, epid) = seed(&pool).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let did = insert_pending(&mut tx, eid, epid, now()).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        record_attempt(
            &pool, did, DeliveryStatus::Delivered, Some(200), Some("ok"), 42, None, 1, now(),
        )
        .await
        .unwrap();

        let log = list_for_event(&pool, eid).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].status, DeliveryStatus::Delivered);
        assert_eq!(log[0].response_status, Some(200));
    }
}