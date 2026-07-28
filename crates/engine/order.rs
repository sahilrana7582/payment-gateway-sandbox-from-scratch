//! Order orchestration. Thin compared to payments — an order is created,
//! read, and listed; its transitions happen as side effects of payments.

use std::collections::HashMap;
use std::sync::Arc;

use domain::clock::Clock;
use domain::id::{MerchantId, OrderId};
use domain::money::{Currency, Money};
use domain::order::Order;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{EngineError, EngineResult};

/// Minimum chargeable amount in minor units, matching the DB CHECK constraint.
pub const MIN_AMOUNT_MINOR: i64 = 100;

pub struct CreateOrderInput {
    pub amount_minor: i64,
    pub currency: Currency,
    pub receipt: Option<String>,
    pub notes: HashMap<String, String>,
}

pub struct OrderService {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl OrderService {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    pub async fn create(
        &self,
        merchant_id: MerchantId,
        input: CreateOrderInput,
    ) -> EngineResult<Order> {
        if input.amount_minor < MIN_AMOUNT_MINOR {
            return Err(EngineError::validation(
                "amount",
                format!("amount must be at least {MIN_AMOUNT_MINOR} minor units"),
            ));
        }
        if input.notes.len() > 15 {
            return Err(EngineError::validation("notes", "at most 15 notes allowed"));
        }
        if let Some(r) = &input.receipt {
            if r.len() > 40 {
                return Err(EngineError::validation("receipt", "at most 40 characters"));
            }
        }

        let amount = Money::new_non_negative(input.amount_minor, input.currency)?;
        let order = Order::new(
            merchant_id,
            amount,
            input.receipt,
            input.notes,
            self.clock.now(),
        );

        store::order::insert(&self.pool, &order).await?;
        Ok(order)
    }

    /// Fetch with an ownership check. Cross-merchant access reads as NotFound.
    pub async fn get(&self, merchant_id: MerchantId, id: OrderId) -> EngineResult<Order> {
        let order = store::order::find_by_id(&self.pool, id).await?;
        if order.merchant_id != merchant_id {
            return Err(EngineError::not_found("order"));
        }
        Ok(order)
    }

    pub async fn list(
        &self,
        merchant_id: MerchantId,
        before: Option<OffsetDateTime>,
        limit: i64,
    ) -> EngineResult<Vec<Order>> {
        let limit = limit.clamp(1, 100);
        Ok(store::order::list_for_merchant(&self.pool, merchant_id, before, limit).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::clock::FixedClock;
    use time::macros::datetime;

    fn service(pool: &PgPool) -> OrderService {
        OrderService::new(
            pool.clone(),
            Arc::new(FixedClock::new(datetime!(2026-01-01 00:00:00 UTC))),
        )
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new(
            "Acme",
            "a@acme.dev",
            datetime!(2026-01-01 00:00:00 UTC),
        )
        .unwrap();
        store::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    fn input(minor: i64) -> CreateOrderInput {
        CreateOrderInput {
            amount_minor: minor,
            currency: Currency::Inr,
            receipt: None,
            notes: HashMap::new(),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn create_and_get(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let svc = service(&pool);
        let order = svc.create(mid, input(150_000)).await.unwrap();
        let fetched = svc.get(mid, order.id).await.unwrap();
        assert_eq!(fetched.id, order.id);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn below_minimum_rejected(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let err = service(&pool).create(mid, input(99)).await.unwrap_err();
        assert!(matches!(err, EngineError::Validation { field: "amount", .. }));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn cross_merchant_access_reads_as_not_found(pool: PgPool) {
        let m1 = seed_merchant(&pool).await;
        let m2 = {
            let m = domain::merchant::Merchant::new(
                "Beta",
                "b@beta.dev",
                datetime!(2026-01-01 00:00:00 UTC),
            )
            .unwrap();
            store::merchant::insert(&pool, &m).await.unwrap();
            m.id
        };
        let svc = service(&pool);
        let order = svc.create(m1, input(150_000)).await.unwrap();
        let err = svc.get(m2, order.id).await.unwrap_err();
        assert!(err.is_not_found());
    }
}