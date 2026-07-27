//! Settlement persistence. A settlement is the T+2 sweep: cleared funds move
//! from merchant_pending to merchant_available and a payout record is written.
//! The reconciliation CSV is generated from the entries in the period.

use domain::id::{MerchantId, SettlementId};
use domain::money::Currency;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::currency_from_column;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "settlement_status", rename_all = "snake_case")]
pub enum SettlementStatus {
    Pending,
    Processed,
}

#[derive(Debug, Clone)]
pub struct Settlement {
    pub id: SettlementId,
    pub merchant_id: MerchantId,
    pub currency: Currency,
    pub period_start: OffsetDateTime,
    pub period_end: OffsetDateTime,
    pub gross_minor: i64,
    pub fees_minor: i64,
    pub tax_minor: i64,
    pub refunds_minor: i64,
    pub net_minor: i64,
    pub status: SettlementStatus,
    pub created_at: OffsetDateTime,
    pub processed_at: Option<OffsetDateTime>,
}

struct SettlementRow {
    id: Uuid,
    merchant_id: Uuid,
    currency: String,
    period_start: OffsetDateTime,
    period_end: OffsetDateTime,
    gross_minor: i64,
    fees_minor: i64,
    tax_minor: i64,
    refunds_minor: i64,
    net_minor: i64,
    status: SettlementStatus,
    created_at: OffsetDateTime,
    processed_at: Option<OffsetDateTime>,
}

impl SettlementRow {
    fn into_domain(self) -> StoreResult<Settlement> {
        Ok(Settlement {
            id: SettlementId::from_uuid(self.id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            currency: currency_from_column(&self.currency)?,
            period_start: self.period_start,
            period_end: self.period_end,
            gross_minor: self.gross_minor,
            fees_minor: self.fees_minor,
            tax_minor: self.tax_minor,
            refunds_minor: self.refunds_minor,
            net_minor: self.net_minor,
            status: self.status,
            created_at: self.created_at,
            processed_at: self.processed_at,
        })
    }
}

pub async fn insert(tx: &mut PgTx<'_>, s: &Settlement) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO settlements
            (id, merchant_id, currency, period_start, period_end,
             gross_minor, fees_minor, tax_minor, refunds_minor, net_minor,
             status, created_at, processed_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
        "#,
        s.id.as_uuid(),
        s.merchant_id.as_uuid(),
        s.currency.code(),
        s.period_start,
        s.period_end,
        s.gross_minor,
        s.fees_minor,
        s.tax_minor,
        s.refunds_minor,
        s.net_minor,
        s.status as SettlementStatus,
        s.created_at,
        s.processed_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn find_by_id(pool: &PgPool, id: SettlementId) -> StoreResult<Settlement> {
    let row = sqlx::query_as!(
        SettlementRow,
        r#"
        SELECT id, merchant_id, currency, period_start, period_end,
               gross_minor, fees_minor, tax_minor, refunds_minor, net_minor,
               status AS "status: SettlementStatus", created_at, processed_at
        FROM settlements WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::not_found("settlement"))?;
    row.into_domain()
}

pub async fn list_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    limit: i64,
) -> StoreResult<Vec<Settlement>> {
    let rows = sqlx::query_as!(
        SettlementRow,
        r#"
        SELECT id, merchant_id, currency, period_start, period_end,
               gross_minor, fees_minor, tax_minor, refunds_minor, net_minor,
               status AS "status: SettlementStatus", created_at, processed_at
        FROM settlements WHERE merchant_id = $1
        ORDER BY period_end DESC LIMIT $2
        "#,
        merchant_id.as_uuid(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(SettlementRow::into_domain).collect()
}

pub async fn mark_processed(
    tx: &mut PgTx<'_>,
    id: SettlementId,
    processed_at: OffsetDateTime,
) -> StoreResult<()> {
    sqlx::query!(
        r#"UPDATE settlements SET status = 'processed', processed_at = $2 WHERE id = $1"#,
        id.as_uuid(),
        processed_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    fn settlement(merchant_id: MerchantId) -> Settlement {
        Settlement {
            id: SettlementId::new(),
            merchant_id,
            currency: Currency::Inr,
            period_start: datetime!(2026-01-01 00:00:00 UTC),
            period_end: datetime!(2026-01-02 00:00:00 UTC),
            gross_minor: 100_000,
            fees_minor: 2_000,
            tax_minor: 360,
            refunds_minor: 0,
            net_minor: 97_640,
            status: SettlementStatus::Pending,
            created_at: now(),
            processed_at: None,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_and_find_roundtrips(pool: PgPool) {
        let merchant_id = seed_merchant(&pool).await;
        let s = settlement(merchant_id);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &s).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, s.id).await.unwrap();
        assert_eq!(found.id, s.id);
        assert_eq!(found.status, SettlementStatus::Pending);
        assert_eq!(found.net_minor, 97_640);
        assert!(found.processed_at.is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_by_id_missing_is_not_found(pool: PgPool) {
        let err = find_by_id(&pool, SettlementId::new()).await.unwrap_err();
        assert!(err.is_not_found());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn mark_processed_updates_status_and_timestamp(pool: PgPool) {
        let merchant_id = seed_merchant(&pool).await;
        let s = settlement(merchant_id);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &s).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let processed_at = datetime!(2026-01-03 00:00:00 UTC);
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        mark_processed(&mut tx, s.id, processed_at).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, s.id).await.unwrap();
        assert_eq!(found.status, SettlementStatus::Processed);
        assert_eq!(found.processed_at, Some(processed_at));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn list_for_merchant_orders_by_period_end_desc(pool: PgPool) {
        let merchant_id = seed_merchant(&pool).await;

        let mut older = settlement(merchant_id);
        older.period_start = datetime!(2026-01-01 00:00:00 UTC);
        older.period_end = datetime!(2026-01-02 00:00:00 UTC);

        let mut newer = settlement(merchant_id);
        newer.period_start = datetime!(2026-01-02 00:00:00 UTC);
        newer.period_end = datetime!(2026-01-03 00:00:00 UTC);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &older).await.unwrap();
        insert(&mut tx, &newer).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let settlements = list_for_merchant(&pool, merchant_id, 10).await.unwrap();
        assert_eq!(settlements.len(), 2);
        assert_eq!(settlements[0].id, newer.id);
        assert_eq!(settlements[1].id, older.id);
    }
}
