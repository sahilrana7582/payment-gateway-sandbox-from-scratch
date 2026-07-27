//! Event log persistence. Events are written in the SAME transaction as the
//! state change that produced them (transactional outbox) — `insert` takes
//! `&mut PgTx`, never a pool. Reads are pool-based for the dashboard.

use domain::id::{EventId, MerchantId};
use serde_json::Value as Json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;
use crate::tx::PgTx;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: EventId,
    pub merchant_id: MerchantId,
    pub event_type: String,
    pub payload: Json,
    pub api_version: String,
    pub created_at: OffsetDateTime,
}

struct EventRow {
    id: Uuid,
    merchant_id: Uuid,
    event_type: String,
    payload: Json,
    api_version: String,
    created_at: OffsetDateTime,
}

impl From<EventRow> for Event {
    fn from(r: EventRow) -> Self {
        Event {
            id: EventId::from_uuid(r.id),
            merchant_id: MerchantId::from_uuid(r.merchant_id),
            event_type: r.event_type,
            payload: r.payload,
            api_version: r.api_version,
            created_at: r.created_at,
        }
    }
}

/// Emit an event inside the caller's transaction. Returns the event id, which
/// the caller pairs with an enqueued webhook-delivery job.
pub async fn insert(
    tx: &mut PgTx<'_>,
    merchant_id: MerchantId,
    event_type: &str,
    payload: &Json,
    api_version: &str,
    now: OffsetDateTime,
) -> StoreResult<EventId> {
    let id = EventId::new();
    sqlx::query!(
        r#"
        INSERT INTO events (id, merchant_id, type, payload, api_version, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        id.as_uuid(),
        merchant_id.as_uuid(),
        event_type,
        payload,
        api_version,
        now,
    )
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

pub async fn find_by_id(pool: &PgPool, id: EventId) -> StoreResult<Event> {
    let row = sqlx::query_as!(
        EventRow,
        r#"
        SELECT id, merchant_id, type AS "event_type", payload, api_version, created_at
        FROM events WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| crate::error::StoreError::not_found("event"))?;
    Ok(row.into())
}

pub async fn list_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    before: Option<OffsetDateTime>,
    limit: i64,
) -> StoreResult<Vec<Event>> {
    let rows = sqlx::query_as!(
        EventRow,
        r#"
        SELECT id, merchant_id, type AS "event_type", payload, api_version, created_at
        FROM events
        WHERE merchant_id = $1
          AND ($2::timestamptz IS NULL OR created_at < $2)
        ORDER BY created_at DESC
        LIMIT $3
        "#,
        merchant_id.as_uuid(),
        before,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Event::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    #[sqlx::test]
    async fn emit_and_read(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let id = insert(
            &mut tx,
            mid,
            "payment.captured",
            &json!({"payment": {"id": "pay_1"}}),
            "2026-07-01",
            now(),
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let ev = find_by_id(&pool, id).await.unwrap();
        assert_eq!(ev.event_type, "payment.captured");
        assert_eq!(ev.api_version, "2026-07-01");
    }
}