use domain::id::{MerchantId, WebhookEndpointId};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{StoreError, StoreResult};

#[derive(Debug, Clone)]
pub struct WebhookEndpoint {
    pub id: WebhookEndpointId,
    pub merchant_id: MerchantId,
    pub url: String,
    pub signing_secret: String,
    pub enabled_events: Vec<String>,
    pub disabled_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

impl WebhookEndpoint {
    pub fn is_active(&self) -> bool {
        self.disabled_at.is_none()
    }

    /// Whether this endpoint wants a given event type. Empty `enabled_events`
    /// means "all events".
    pub fn wants(&self, event_type: &str) -> bool {
        self.enabled_events.is_empty() || self.enabled_events.iter().any(|e| e == event_type)
    }
}

struct EndpointRow {
    id: Uuid,
    merchant_id: Uuid,
    url: String,
    signing_secret: String,
    enabled_events: Vec<String>,
    disabled_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl From<EndpointRow> for WebhookEndpoint {
    fn from(r: EndpointRow) -> Self {
        WebhookEndpoint {
            id: WebhookEndpointId::from_uuid(r.id),
            merchant_id: MerchantId::from_uuid(r.merchant_id),
            url: r.url,
            signing_secret: r.signing_secret,
            enabled_events: r.enabled_events,
            disabled_at: r.disabled_at,
            created_at: r.created_at,
        }
    }
}

pub async fn insert(pool: &PgPool, ep: &WebhookEndpoint) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO webhook_endpoints
            (id, merchant_id, url, signing_secret, enabled_events, disabled_at, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        ep.id.as_uuid(),
        ep.merchant_id.as_uuid(),
        ep.url,
        ep.signing_secret,
        &ep.enabled_events,
        ep.disabled_at,
        ep.created_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_by_id<'e, E>(executor: E, id: WebhookEndpointId) -> StoreResult<WebhookEndpoint>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        EndpointRow,
        r#"
        SELECT id, merchant_id, url, signing_secret, enabled_events, disabled_at, created_at
        FROM webhook_endpoints WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("webhook_endpoint"))?;
    Ok(row.into())
}

/// Active endpoints for a merchant — the delivery fan-out list.
pub async fn list_active_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
) -> StoreResult<Vec<WebhookEndpoint>> {
    let rows = sqlx::query_as!(
        EndpointRow,
        r#"
        SELECT id, merchant_id, url, signing_secret, enabled_events, disabled_at, created_at
        FROM webhook_endpoints
        WHERE merchant_id = $1 AND disabled_at IS NULL
        ORDER BY created_at
        "#,
        merchant_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(WebhookEndpoint::from).collect())
}

pub async fn set_disabled(
    pool: &PgPool,
    id: WebhookEndpointId,
    disabled_at: Option<OffsetDateTime>,
) -> StoreResult<()> {
    sqlx::query!(
        r#"UPDATE webhook_endpoints SET disabled_at = $2 WHERE id = $1"#,
        id.as_uuid(),
        disabled_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::id::WebhookEndpointId;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    fn ep(merchant_id: MerchantId) -> WebhookEndpoint {
        WebhookEndpoint {
            id: WebhookEndpointId::new(),
            merchant_id,
            url: "https://shop.acme.dev/webhooks".into(),
            signing_secret: "whsec_test".into(),
            enabled_events: vec![],
            disabled_at: None,
            created_at: now(),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_and_list_active(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        insert(&pool, &ep(mid)).await.unwrap();
        let active = list_active_for_merchant(&pool, mid).await.unwrap();
        assert_eq!(active.len(), 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn disabled_endpoint_excluded_from_active(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let e = ep(mid);
        insert(&pool, &e).await.unwrap();
        set_disabled(&pool, e.id, Some(now())).await.unwrap();
        let active = list_active_for_merchant(&pool, mid).await.unwrap();
        assert_eq!(active.len(), 0);
    }

    #[test]
    fn wants_respects_empty_as_all() {
        let mut e = ep(MerchantId::new());
        assert!(e.wants("payment.captured")); // empty = all
        e.enabled_events = vec!["refund.processed".into()];
        assert!(!e.wants("payment.captured"));
        assert!(e.wants("refund.processed"));
    }
}
