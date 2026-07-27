//! Order persistence. Follows the reference pattern from `merchant.rs`:
//! private row struct + `into_domain`, executor-generic reads, typed errors.
//!
//! New here: `find_by_id_for_update` takes a `&mut PgTx` and locks the row
//! with `SELECT ... FOR UPDATE`. Per the lock-ordering rule in `tx.rs`, the
//! order lock is rank 2 — always taken AFTER a payment lock (rank 1) and
//! BEFORE any ledger account lock (rank 5) within the same transaction.

use std::collections::HashMap;

use domain::id::{MerchantId, OrderId};
use domain::order::Order;
use domain::order_status::OrderStatus;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::money_from_columns;

/// Raw row shape. One place that knows the column layout; every read funnels
/// through `into_domain` so adding a column is a single-site change.
struct OrderRow {
    id: Uuid,
    merchant_id: Uuid,
    amount_minor: i64,
    currency: String,
    amount_paid: i64,
    receipt: Option<String>,
    notes: serde_json::Value,
    status: OrderStatus,
    created_at: OffsetDateTime,
}

impl OrderRow {
    fn into_domain(self) -> StoreResult<Order> {
        let amount = money_from_columns(self.amount_minor, &self.currency)?;
        let notes: HashMap<String, String> = serde_json::from_value(self.notes)
            .map_err(|e| StoreError::decode("order.notes", e.to_string()))?;

        Ok(Order {
            id: OrderId::from_uuid(self.id),
            merchant_id: MerchantId::from_uuid(self.merchant_id),
            amount,
            amount_paid: self.amount_paid,
            receipt: self.receipt,
            notes,
            status: self.status,
            created_at: self.created_at,
        })
    }
}

pub async fn insert(pool: &PgPool, order: &Order) -> StoreResult<()> {
    let notes = serde_json::to_value(&order.notes)
        .map_err(|e| StoreError::decode("order.notes", e.to_string()))?;

    sqlx::query!(
        r#"
        INSERT INTO orders
            (id, merchant_id, amount_minor, currency, amount_paid,
             receipt, notes, status, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
        order.id.as_uuid(),
        order.merchant_id.as_uuid(),
        order.amount.minor(),
        order.amount.currency().code(),
        order.amount_paid,
        order.receipt,
        notes,
        order.status as OrderStatus,
        order.created_at,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Read a single order, not locked. Executor-generic so it works against a
/// `&PgPool` for plain reads AND inside a `&mut PgTx` when a larger transaction
/// needs the row without locking it.
pub async fn find_by_id<'e, E>(executor: E, id: OrderId) -> StoreResult<Order>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        OrderRow,
        r#"
        SELECT id, merchant_id, amount_minor, currency, amount_paid,
               receipt, notes, status AS "status: OrderStatus", created_at
        FROM orders
        WHERE id = $1
        "#,
        id.as_uuid(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("order"))?;

    row.into_domain()
}

/// Read an order and take a row lock for the duration of the transaction.
///
/// This is the concurrency-safe read used before mutating an order (e.g. when
/// a payment against it captures and we must transition Created/Attempted →
/// Paid). Two concurrent payments on the same order serialize here: the second
/// blocks until the first commits, then sees the updated state.
///
/// LOCK ORDER: `orders` is rank 2. If this transaction also locks a payment
/// row, lock the PAYMENT first (rank 1). See `tx::LockOrder`.
pub async fn find_by_id_for_update(tx: &mut PgTx<'_>, id: OrderId) -> StoreResult<Order> {
    let row = sqlx::query_as!(
        OrderRow,
        r#"
        SELECT id, merchant_id, amount_minor, currency, amount_paid,
               receipt, notes, status AS "status: OrderStatus", created_at
        FROM orders
        WHERE id = $1
        FOR UPDATE
        "#,
        id.as_uuid(),
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| StoreError::not_found("order"))?;

    row.into_domain()
}

/// Persist a status + amount_paid change. Runs inside the caller's transaction
/// so it commits atomically with whatever else that transaction is doing (the
/// payment capture, the ledger postings, the event).
pub async fn update_status(
    tx: &mut PgTx<'_>,
    id: OrderId,
    status: OrderStatus,
    amount_paid: i64,
) -> StoreResult<()> {
    let affected = sqlx::query!(
        r#"
        UPDATE orders
        SET status = $2, amount_paid = $3
        WHERE id = $1
        "#,
        id.as_uuid(),
        status as OrderStatus,
        amount_paid,
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(StoreError::not_found("order"));
    }
    Ok(())
}

/// Paginated list for the dashboard / API. Cursor is the `created_at` of the
/// last row seen; `None` starts from the newest.
pub async fn list_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    before: Option<OffsetDateTime>,
    limit: i64,
) -> StoreResult<Vec<Order>> {
    let rows = sqlx::query_as!(
        OrderRow,
        r#"
        SELECT id, merchant_id, amount_minor, currency, amount_paid,
               receipt, notes, status AS "status: OrderStatus", created_at
        FROM orders
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

    rows.into_iter().map(OrderRow::into_domain).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::money::{Currency, Money};
    use time::macros::datetime;

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new(
            "Acme",
            "a@acme.dev",
            datetime!(2026-01-01 00:00:00 UTC),
        )
        .unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    fn sample_order(merchant_id: MerchantId) -> Order {
        Order::new(
            merchant_id,
            Money::new(150_000, Currency::Inr),
            Some("receipt_1".into()),
            HashMap::new(),
            datetime!(2026-01-01 00:00:00 UTC),
        )
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn insert_then_find(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let order = sample_order(mid);
        insert(&pool, &order).await.unwrap();

        let found = find_by_id(&pool, order.id).await.unwrap();
        assert_eq!(found.amount, Money::new(150_000, Currency::Inr));
        assert_eq!(found.status, OrderStatus::Created);
        assert_eq!(found.receipt.as_deref(), Some("receipt_1"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn duplicate_receipt_for_same_merchant_conflicts(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let o1 = sample_order(mid);
        insert(&pool, &o1).await.unwrap();

        let mut o2 = sample_order(mid);
        o2 = Order {
            id: OrderId::new(),
            ..o2
        }; // same receipt, new id
        let err = insert(&pool, &o2).await.unwrap_err();
        assert!(err.is_conflict());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn update_status_inside_transaction(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let order = sample_order(mid);
        insert(&pool, &order).await.unwrap();

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let locked = find_by_id_for_update(&mut tx, order.id).await.unwrap();
        assert_eq!(locked.status, OrderStatus::Created);

        update_status(&mut tx, order.id, OrderStatus::Paid, 150_000)
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();

        let found = find_by_id(&pool, order.id).await.unwrap();
        assert_eq!(found.status, OrderStatus::Paid);
        assert_eq!(found.amount_paid, 150_000);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn notes_survive_roundtrip(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let mut notes = HashMap::new();
        notes.insert("customer_id".to_string(), "42".to_string());
        let order = Order::new(
            mid,
            Money::new(100_000, Currency::Inr),
            None,
            notes.clone(),
            datetime!(2026-01-01 00:00:00 UTC),
        );
        insert(&pool, &order).await.unwrap();

        let found = find_by_id(&pool, order.id).await.unwrap();
        assert_eq!(found.notes, notes);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn list_respects_limit_and_order(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        for i in 0..3 {
            let o = Order::new(
                mid,
                Money::new(100_000 + i, Currency::Inr),
                None,
                HashMap::new(),
                datetime!(2026-01-01 00:00:00 UTC) + time::Duration::seconds(i),
            );
            insert(&pool, &o).await.unwrap();
        }
        let list = list_for_merchant(&pool, mid, None, 2).await.unwrap();
        assert_eq!(list.len(), 2);
        // DESC order: newest first
        assert!(list[0].created_at >= list[1].created_at);
    }
}
