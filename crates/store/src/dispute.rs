//! Dispute persistence. A dispute freezes funds until resolved; the evidence
//! deadline is the pressure that makes disputes worth teaching.

use domain::id::{DisputeId, MerchantId, PaymentId};
use domain::money::Money;
use serde_json::Value as Json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::money_from_columns;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "dispute_reason", rename_all = "snake_case")]
pub enum DisputeReason {
    Fraudulent,
    Duplicate,
    ProductNotReceived,
    ProductUnacceptable,
    SubscriptionCancelled,
    Unrecognized,
    CreditNotProcessed,
    General,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "dispute_status", rename_all = "snake_case")]
pub enum DisputeStatus {
    Open,
    UnderReview,
    Won,
    Lost,
}

#[derive(Debug, Clone)]
pub struct Dispute {
    pub id: DisputeId,
    pub payment_id: PaymentId,
    pub merchant_id: MerchantId,
    pub amount: Money,
    pub reason: DisputeReason,
    pub status: DisputeStatus,
    pub evidence: Json,
    pub respond_by: OffsetDateTime,
    pub created_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
}

struct DisputeRow {
    id: Uuid,
    payment_id: Uuid,
    merchant_id: Uuid,
    amount_minor: i64,
    currency: String,
    reason: DisputeReason,
    status: DisputeStatus,
    evidence: Json,
    respond_by: OffsetDateTime,
    created_at: OffsetDateTime,
    resolved_at: Option<OffsetDateTime>,
}

impl DisputeRow {
    fn into_domain(self) -> StoreResult<Dispute> {
        Ok(Dispute {
            id: DisputeId::from_uuid(self.id),
            payment_id: PaymentId::from_uuid(self.payment_id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            amount: money_from_columns(self.amount_minor, &self.currency)?,
            reason: self.reason,
            status: self.status,
            evidence: self.evidence,
            respond_by: self.respond_by,
            created_at: self.created_at,
            resolved_at: self.resolved_at,
        })
    }
}

pub async fn insert(tx: &mut PgTx<'_>, d: &Dispute) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO disputes
            (id, payment_id, merchant_id, amount_minor, currency, reason,
             status, evidence, respond_by, created_at, resolved_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        "#,
        d.id.as_uuid(),
        d.payment_id.as_uuid(),
        d.merchant_id.as_uuid(),
        d.amount.minor(),
        d.amount.currency().code(),
        d.reason as DisputeReason,
        d.status as DisputeStatus,
        d.evidence,
        d.respond_by,
        d.created_at,
        d.resolved_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn find_by_id<'e, E>(executor: E, id: DisputeId) -> StoreResult<Dispute>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        DisputeRow,
        r#"
        SELECT id, payment_id, merchant_id, amount_minor, currency,
               reason AS "reason: DisputeReason", status AS "status: DisputeStatus",
               evidence, respond_by, created_at, resolved_at
        FROM disputes WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("dispute"))?;
    row.into_domain()
}

pub async fn find_by_id_for_update(tx: &mut PgTx<'_>, id: DisputeId) -> StoreResult<Dispute> {
    let row = sqlx::query_as!(
        DisputeRow,
        r#"
        SELECT id, payment_id, merchant_id, amount_minor, currency,
               reason AS "reason: DisputeReason", status AS "status: DisputeStatus",
               evidence, respond_by, created_at, resolved_at
        FROM disputes WHERE id = $1 FOR UPDATE
        "#,
        id.as_uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::not_found("dispute"))?;
    row.into_domain()
}

pub async fn update_status(
    tx: &mut PgTx<'_>,
    id: DisputeId,
    status: DisputeStatus,
    evidence: Option<&Json>,
    resolved_at: Option<OffsetDateTime>,
) -> StoreResult<()> {
    sqlx::query!(
        r#"
        UPDATE disputes
        SET status = $2,
            evidence = COALESCE($3, evidence),
            resolved_at = $4
        WHERE id = $1
        "#,
        id.as_uuid(),
        status as DisputeStatus,
        evidence,
        resolved_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn list_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    limit: i64,
) -> StoreResult<Vec<Dispute>> {
    let rows = sqlx::query_as!(
        DisputeRow,
        r#"
        SELECT id, payment_id, merchant_id, amount_minor, currency,
               reason AS "reason: DisputeReason", status AS "status: DisputeStatus",
               evidence, respond_by, created_at, resolved_at
        FROM disputes WHERE merchant_id = $1
        ORDER BY created_at DESC LIMIT $2
        "#,
        merchant_id.as_uuid(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(DisputeRow::into_domain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::money::Currency;
    use std::collections::HashMap;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    fn respond_by() -> OffsetDateTime {
        datetime!(2026-01-08 00:00:00 UTC)
    }

    async fn seed_payment(pool: &PgPool) -> (MerchantId, PaymentId) {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        let o = domain::order::Order::new(
            m.id,
            Money::new(150_000, Currency::Inr),
            None,
            HashMap::new(),
            now(),
        );
        crate::order::insert(pool, &o).await.unwrap();

        let card = domain::card::CardDetails {
            last4: "4242".into(),
            brand: domain::card::CardBrand::Visa,
            exp_month: 12,
            exp_year: 2030,
            fingerprint: "fp".into(),
        };
        let p = domain::payment::Payment::new_card_attempt(
            o.id,
            m.id,
            Money::new(150_000, Currency::Inr),
            card,
            HashMap::new(),
            now(),
        );
        let mut tx = crate::tx::begin(pool).await.unwrap();
        crate::payment::insert(&mut tx, &p).await.unwrap();
        crate::tx::commit(tx).await.unwrap();
        (m.id, p.id)
    }

    fn dispute(pid: PaymentId, mid: MerchantId) -> Dispute {
        Dispute {
            id: DisputeId::new(),
            payment_id: pid,
            merchant_id: mid,
            amount: Money::new(150_000, Currency::Inr),
            reason: DisputeReason::Fraudulent,
            status: DisputeStatus::Open,
            evidence: serde_json::json!({}),
            respond_by: respond_by(),
            created_at: now(),
            resolved_at: None,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_and_find_roundtrips(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;
        let d = dispute(pid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &d).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, d.id).await.unwrap();
        assert_eq!(found.id, d.id);
        assert_eq!(found.status, DisputeStatus::Open);
        assert_eq!(found.reason, DisputeReason::Fraudulent);
        assert_eq!(found.amount, d.amount);
        assert!(found.resolved_at.is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_by_id_missing_is_not_found(pool: PgPool) {
        let err = find_by_id(&pool, DisputeId::new()).await.unwrap_err();
        assert!(err.is_not_found());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn update_status_resolves_with_evidence(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;
        let d = dispute(pid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &d).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let new_evidence = serde_json::json!({"note": "shipped with tracking"});
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        update_status(
            &mut tx,
            d.id,
            DisputeStatus::Won,
            Some(&new_evidence),
            Some(now()),
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, d.id).await.unwrap();
        assert_eq!(found.status, DisputeStatus::Won);
        assert_eq!(found.evidence, new_evidence);
        assert_eq!(found.resolved_at, Some(now()));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn update_status_without_evidence_keeps_existing(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;
        let mut d = dispute(pid, mid);
        d.evidence = serde_json::json!({"note": "original"});

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &d).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        update_status(&mut tx, d.id, DisputeStatus::UnderReview, None, None)
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, d.id).await.unwrap();
        assert_eq!(found.status, DisputeStatus::UnderReview);
        assert_eq!(found.evidence, d.evidence);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_by_id_for_update_locks_row(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;
        let d = dispute(pid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &d).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let found = find_by_id_for_update(&mut tx, d.id).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(found.id, d.id);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn list_for_merchant_orders_by_created_at_desc(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;

        let mut older = dispute(pid, mid);
        older.created_at = datetime!(2026-01-01 00:00:00 UTC);
        let mut newer = dispute(pid, mid);
        newer.created_at = datetime!(2026-01-02 00:00:00 UTC);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &older).await.unwrap();
        insert(&mut tx, &newer).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let disputes = list_for_merchant(&pool, mid, 10).await.unwrap();
        assert_eq!(disputes.len(), 2);
        assert_eq!(disputes[0].id, newer.id);
        assert_eq!(disputes[1].id, older.id);
    }
}
