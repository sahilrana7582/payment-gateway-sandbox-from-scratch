//! Payment persistence. Largest repository — a payment carries inline card
//! metadata (never a PAN), an error code on failure, and a capture timestamp.
//!
//! `find_by_id_for_update` is rank 1 in the lock order (`tx::LockOrder`): when
//! a transaction locks both a payment and its order, the PAYMENT is locked
//! first, then the order. Every path obeys this so concurrent captures can't
//! deadlock.

use std::collections::HashMap;

use domain::card::{CardBrand, CardDetails};
use domain::id::{MerchantId, OrderId, PaymentId};
use domain::payment::Payment;
use domain::payment_method::PaymentMethodType;
use domain::payment_status::PaymentStatus;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::money_from_columns;

struct PaymentRow {
    id: Uuid,
    order_id: Uuid,
    merchant_id: Uuid,
    amount_minor: i64,
    currency: String,
    amount_refunded: i64,
    method: PaymentMethodType,
    card_last4: Option<String>,
    card_brand: Option<CardBrand>,
    card_exp_month: Option<i16>,
    card_exp_year: Option<i16>,
    card_fingerprint: Option<String>,
    status: PaymentStatus,
    error_code: Option<String>,
    error_description: Option<String>,
    notes: serde_json::Value,
    created_at: OffsetDateTime,
    captured_at: Option<OffsetDateTime>,
}

impl PaymentRow {
    fn into_domain(self) -> StoreResult<Payment> {
        let amount = money_from_columns(self.amount_minor, &self.currency)?;

        // Reassemble card metadata only if all required parts are present.
        let card = match (
            self.card_last4,
            self.card_brand,
            self.card_exp_month,
            self.card_exp_year,
            self.card_fingerprint,
        ) {
            (Some(last4), Some(brand), Some(m), Some(y), Some(fp)) => Some(CardDetails {
                last4,
                brand,
                exp_month: m as u8,
                exp_year: y as u16,
                fingerprint: fp,
            }),
            _ => None,
        };

        let notes: HashMap<String, String> = serde_json::from_value(self.notes)
            .map_err(|e| StoreError::decode("payment.notes", e.to_string()))?;

        Ok(Payment {
            id: PaymentId::from_uuid(self.id),
            order_id: OrderId::from_uuid(self.order_id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            amount,
            amount_refunded: self.amount_refunded,
            method: self.method,
            card,
            status: self.status,
            error_code: self.error_code,
            error_description: self.error_description,
            notes,
            created_at: self.created_at,
            captured_at: self.captured_at,
        })
    }
}

/// Insert a payment. Runs inside the caller's transaction because a new payment
/// is normally created together with the order transition to `Attempted` and
/// (on auto-capture success) the ledger postings — all one atomic unit.
pub async fn insert(tx: &mut PgTx<'_>, payment: &Payment) -> StoreResult<()> {
    let notes = serde_json::to_value(&payment.notes)
        .map_err(|e| StoreError::decode("payment.notes", e.to_string()))?;

    let (last4, brand, exp_m, exp_y, fp) = match &payment.card {
        Some(c) => (
            Some(c.last4.clone()),
            Some(c.brand),
            Some(c.exp_month as i16),
            Some(c.exp_year as i16),
            Some(c.fingerprint.clone()),
        ),
        None => (None, None, None, None, None),
    };

    sqlx::query!(
        r#"
        INSERT INTO payments
            (id, order_id, merchant_id, amount_minor, currency, amount_refunded,
             method, card_last4, card_brand, card_exp_month, card_exp_year,
             card_fingerprint, status, error_code, error_description, notes,
             created_at, captured_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18)
        "#,
        payment.id.as_uuid(),
        payment.order_id.as_uuid(),
        payment.merchant_id.as_uuid(),
        payment.amount.minor(),
        payment.amount.currency().code(),
        payment.amount_refunded,
        payment.method as PaymentMethodType,
        last4,
        brand as Option<CardBrand>,
        exp_m,
        exp_y,
        fp,
        payment.status as PaymentStatus,
        payment.error_code,
        payment.error_description,
        notes,
        payment.created_at,
        payment.captured_at,
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Unlocked read, executor-generic (works on `&PgPool` or inside a `&mut PgTx`).
pub async fn find_by_id<'e, E>(executor: E, id: PaymentId) -> StoreResult<Payment>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        PaymentRow,
        r#"
        SELECT id, order_id, merchant_id, amount_minor, currency, amount_refunded,
               method AS "method: PaymentMethodType",
               card_last4, card_brand AS "card_brand: CardBrand",
               card_exp_month, card_exp_year, card_fingerprint,
               status AS "status: PaymentStatus",
               error_code, error_description, notes, created_at, captured_at
        FROM payments
        WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("payment"))?;

    row.into_domain()
}

/// Locked read for mutation. RANK 1 in the lock order — lock the payment
/// BEFORE the order (rank 2) when a transaction touches both. Used before
/// capture and before refund.
pub async fn find_by_id_for_update(tx: &mut PgTx<'_>, id: PaymentId) -> StoreResult<Payment> {
    let row = sqlx::query_as!(
        PaymentRow,
        r#"
        SELECT id, order_id, merchant_id, amount_minor, currency, amount_refunded,
               method AS "method: PaymentMethodType",
               card_last4, card_brand AS "card_brand: CardBrand",
               card_exp_month, card_exp_year, card_fingerprint,
               status AS "status: PaymentStatus",
               error_code, error_description, notes, created_at, captured_at
        FROM payments
        WHERE id = $1
        FOR UPDATE
        "#,
        id.as_uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::not_found("payment"))?;

    row.into_domain()
}

/// Persist a status transition plus its associated fields. One update covers
/// capture (status + captured_at), failure (status + error), and refund
/// (status + amount_refunded), so the caller passes whichever apply.
pub async fn update_status(
    tx: &mut PgTx<'_>,
    id: PaymentId,
    status: PaymentStatus,
    captured_at: Option<OffsetDateTime>,
    amount_refunded: i64,
    error_code: Option<&str>,
    error_description: Option<&str>,
) -> StoreResult<()> {
    let affected = sqlx::query!(
        r#"
        UPDATE payments
        SET status = $2,
            captured_at = $3,
            amount_refunded = $4,
            error_code = $5,
            error_description = $6
        WHERE id = $1
        "#,
        id.as_uuid(),
        status as PaymentStatus,
        captured_at,
        amount_refunded,
        error_code,
        error_description,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(StoreError::not_found("payment"));
    }
    Ok(())
}

/// All payments for an order (the attempt history). Newest first.
pub async fn list_for_order(pool: &PgPool, order_id: OrderId) -> StoreResult<Vec<Payment>> {
    let rows = sqlx::query_as!(
        PaymentRow,
        r#"
        SELECT id, order_id, merchant_id, amount_minor, currency, amount_refunded,
               method AS "method: PaymentMethodType",
               card_last4, card_brand AS "card_brand: CardBrand",
               card_exp_month, card_exp_year, card_fingerprint,
               status AS "status: PaymentStatus",
               error_code, error_description, notes, created_at, captured_at
        FROM payments
        WHERE order_id = $1
        ORDER BY created_at DESC
        "#,
        order_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(PaymentRow::into_domain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::money::{Currency, Money};
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_order(pool: &PgPool) -> (MerchantId, OrderId) {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        let order = domain::order::Order::new(
            m.id,
            Money::new(150_000, Currency::Inr),
            None,
            HashMap::new(),
            now(),
        );
        crate::order::insert(pool, &order).await.unwrap();
        (m.id, order.id)
    }

    fn card() -> CardDetails {
        CardDetails {
            last4: "4242".into(),
            brand: CardBrand::Visa,
            exp_month: 12,
            exp_year: 2030,
            fingerprint: "fp_test".into(),
        }
    }

    fn sample_payment(order_id: OrderId, merchant_id: MerchantId) -> Payment {
        Payment::new_card_attempt(
            order_id,
            merchant_id,
            Money::new(150_000, Currency::Inr),
            card(),
            HashMap::new(),
            now(),
        )
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_then_find(pool: PgPool) {
        let (mid, oid) = seed_order(&pool).await;
        let p = sample_payment(oid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &p).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, p.id).await.unwrap();
        assert_eq!(found.status, PaymentStatus::Created);
        assert_eq!(found.card.unwrap().last4, "4242");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn capture_updates_status_and_timestamp(pool: PgPool) {
        let (mid, oid) = seed_order(&pool).await;
        let p = sample_payment(oid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &p).await.unwrap();
        crate::tx::commit(tx).await.unwrap();

        let cap_time = datetime!(2026-01-01 00:05:00 UTC);
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let locked = find_by_id_for_update(&mut tx, p.id).await.unwrap();
        assert_eq!(locked.status, PaymentStatus::Created);
        update_status(
            &mut tx,
            p.id,
            PaymentStatus::Captured,
            Some(cap_time),
            0,
            None,
            None,
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, p.id).await.unwrap();
        assert_eq!(found.status, PaymentStatus::Captured);
        assert_eq!(found.captured_at, Some(cap_time));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn failure_persists_error_code(pool: PgPool) {
        let (mid, oid) = seed_order(&pool).await;
        let p = sample_payment(oid, mid);

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &p).await.unwrap();
        update_status(
            &mut tx,
            p.id,
            PaymentStatus::Failed,
            None,
            0,
            Some("card_declined"),
            Some("The card was declined."),
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, p.id).await.unwrap();
        assert_eq!(found.status, PaymentStatus::Failed);
        assert_eq!(found.error_code.as_deref(), Some("card_declined"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn list_for_order_returns_all_attempts(pool: PgPool) {
        let (mid, oid) = seed_order(&pool).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        for _ in 0..2 {
            let p = sample_payment(oid, mid);
            insert(&mut tx, &p).await.unwrap();
        }
        crate::tx::commit(tx).await.unwrap();

        let attempts = list_for_order(&pool, oid).await.unwrap();
        assert_eq!(attempts.len(), 2);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn captured_at_constraint_rejects_inconsistent_state(pool: PgPool) {
        // The pay_captured_at_consistency CHECK requires captured_at to be set
        // iff status is captured/refunded. Try to violate it directly.
        let (mid, oid) = seed_order(&pool).await;
        let p = sample_payment(oid, mid);
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        insert(&mut tx, &p).await.unwrap();
        // status stays Created but we set captured_at — must be rejected.
        let result = update_status(
            &mut tx,
            p.id,
            PaymentStatus::Created,
            Some(now()),
            0,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
    }
}
