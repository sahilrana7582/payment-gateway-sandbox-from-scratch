//! Audit log persistence. Append-only record of every sensitive action:
//! key created/revoked, refund issued, webhook endpoint changed. Separate from
//! both the ledger (money) and events (merchant-facing) — this is the
//! security trail answering "who did what, when, from where".
//!
//! Writes are fire-and-forget from the caller's perspective: an audit failure
//! must never fail the underlying operation, so callers log the error and
//! continue rather than propagating.

use domain::id::MerchantId;
use serde_json::Value as Json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::StoreResult;

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub merchant_id: Option<MerchantId>,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub before_state: Option<Json>,
    pub after_state: Option<Json>,
    pub ip_address: Option<String>,
}

pub async fn record(pool: &PgPool, entry: &AuditEntry, now: OffsetDateTime) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO audit_logs
            (id, merchant_id, actor_type, actor_id, action, resource_type,
             resource_id, before_state, after_state, ip_address, created_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
        "#,
        Uuid::now_v7(),
        entry.merchant_id.map(|m| m.into_uuid()),
        entry.actor_type,
        entry.actor_id,
        entry.action,
        entry.resource_type,
        entry.resource_id,
        entry.before_state,
        entry.after_state,
        entry.ip_address,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub action: String,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub ip_address: Option<String>,
    pub created_at: OffsetDateTime,
}

pub async fn list_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    limit: i64,
) -> StoreResult<Vec<AuditRecord>> {
    let rows = sqlx::query_as!(
        AuditRecord,
        r#"
        SELECT action, actor_type, actor_id, resource_type, resource_id,
               ip_address, created_at
        FROM audit_logs
        WHERE merchant_id = $1
        ORDER BY created_at DESC
        LIMIT $2
        "#,
        merchant_id.as_uuid(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn record_and_list(pool: PgPool) {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(&pool, &m).await.unwrap();

        record(
            &pool,
            &AuditEntry {
                merchant_id: Some(m.id),
                actor_type: "api_key".into(),
                actor_id: Some("key_1".into()),
                action: "refund.issued".into(),
                resource_type: Some("refund".into()),
                resource_id: Some("re_1".into()),
                before_state: None,
                after_state: None,
                ip_address: Some("203.0.113.1".into()),
            },
            now(),
        )
        .await
        .unwrap();

        let entries = list_for_merchant(&pool, m.id, 10).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "refund.issued");
    }
}
