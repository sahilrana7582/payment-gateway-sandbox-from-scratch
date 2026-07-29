//! Refund persistence.
//!
//! The important function here is `sum_for_payment`, which the engine calls
//! INSIDE the transaction that already holds a `FOR UPDATE` lock on the payment
//! row. That combination is what prevents over-refunding under concurrency:
//! two simultaneous refund requests serialize on the payment lock, so the
//! second one sees the first one's row when it sums.
//!
//! We deliberately do NOT enforce this with a database CHECK — a CHECK can't
//! query sibling rows. Application-level enforcement under a row lock is the
//! correct pattern, and demonstrating it is part of the teaching value.

use domain::id::{MerchantId, PaymentId, RefundId};
use domain::money::Money;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::money_from_columns;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "refund_status", rename_all = "snake_case")]
pub enum RefundStatus {
    Pending,
    Processed,
    Failed,
}

impl RefundStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processed => "processed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Refund {
    pub id: RefundId,
    pub payment_id: PaymentId,
    pub merchant_id: MerchantId,
    pub amount: Money,
    pub reason: Option<String>,
    pub status: RefundStatus,
    pub created_at: OffsetDateTime,
    pub processed_at: Option<OffsetDateTime>,
}

struct RefundRow {
    id: Uuid,
    payment_id: Uuid,
    merchant_id: Uuid,
    amount_minor: i64,
    currency: String,
    reason: Option<String>,
    status: RefundStatus,
    created_at: OffsetDateTime,
    processed_at: Option<OffsetDateTime>,
}

impl RefundRow {
    fn into_domain(self) -> StoreResult<Refund> {
        Ok(Refund {
            id: RefundId::from_uuid(self.id),
            payment_id: PaymentId::from_uuid(self.payment_id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            amount: money_from_columns(self.amount_minor, &self.currency)?,
            reason: self.reason,
            status: self.status,
            created_at: self.created_at,
            processed_at: self.processed_at,
        })
    }
}

/// Insert inside the caller's transaction — a refund is always created
/// together with its ledger reversal and the payment status update.
pub async fn insert(tx: &mut PgTx<'_>, refund: &Refund) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO refunds
            (id, payment_id, merchant_id, amount_minor, currency, reason,
             status, notes, created_at, processed_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, '{}', $8, $9)
        "#,
        refund.id.as_uuid(),
        refund.payment_id.as_uuid(),
        refund.merchant_id.as_uuid(),
        refund.amount.minor(),
        refund.amount.currency().code(),
        refund.reason,
        refund.status as RefundStatus,
        refund.created_at,
        refund.processed_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Total already refunded against a payment, excluding failed refunds.
///
/// MUST be called inside a transaction holding `FOR UPDATE` on the payment row
/// (see `payment::find_by_id_for_update`, lock rank 1). Without that lock, two
/// concurrent refunds could each read the same total and both pass the
/// over-refund check.
pub async fn sum_for_payment(tx: &mut PgTx<'_>, payment_id: PaymentId) -> StoreResult<i64> {
    let row = sqlx::query!(
        r#"
        SELECT COALESCE(SUM(amount_minor), 0)::bigint AS "total!"
        FROM refunds
        WHERE payment_id = $1 AND status <> 'failed'
        "#,
        payment_id.as_uuid(),
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.total)
}

pub async fn find_by_id<'e, E>(executor: E, id: RefundId) -> StoreResult<Refund>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        RefundRow,
        r#"
        SELECT id, payment_id, merchant_id, amount_minor, currency, reason,
               status AS "status: RefundStatus", created_at, processed_at
        FROM refunds WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("refund"))?;
    row.into_domain()
}

pub async fn list_for_payment(pool: &PgPool, payment_id: PaymentId) -> StoreResult<Vec<Refund>> {
    let rows = sqlx::query_as!(
        RefundRow,
        r#"
        SELECT id, payment_id, merchant_id, amount_minor, currency, reason,
               status AS "status: RefundStatus", created_at, processed_at
        FROM refunds WHERE payment_id = $1 ORDER BY created_at DESC
        "#,
        payment_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(RefundRow::into_domain).collect()
}

pub async fn update_status(
    tx: &mut PgTx<'_>,
    id: RefundId,
    status: RefundStatus,
    processed_at: Option<OffsetDateTime>,
) -> StoreResult<()> {
    sqlx::query!(
        r#"UPDATE refunds SET status = $2, processed_at = $3 WHERE id = $1"#,
        id.as_uuid(),
        status as RefundStatus,
        processed_at,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
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

    fn refund(pid: PaymentId, mid: MerchantId, minor: i64) -> Refund {
        Refund {
            id: RefundId::new(),
            payment_id: pid,
            merchant_id: mid,
            amount: Money::new(minor, Currency::Inr),
            reason: Some("customer request".into()),
            status: RefundStatus::Processed,
            created_at: now(),
            processed_at: Some(now()),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn sum_accumulates_partial_refunds(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &refund(pid, mid, 50_000)).await.unwrap();
        insert(&mut tx, &refund(pid, mid, 30_000)).await.unwrap();
        let total = sum_for_payment(&mut tx, pid).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(total, 80_000);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn failed_refunds_excluded_from_sum(pool: PgPool) {
        let (mid, pid) = seed_payment(&pool).await;
        let mut r = refund(pid, mid, 50_000);
        r.status = RefundStatus::Failed;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &r).await.unwrap();
        let total = sum_for_payment(&mut tx, pid).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(total, 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn sum_of_no_refunds_is_zero(pool: PgPool) {
        let (_, pid) = seed_payment(&pool).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        assert_eq!(sum_for_payment(&mut tx, pid).await.unwrap(), 0);
        crate::tx::commit(tx).await.unwrap();
    }
}
