//! Payment orchestration — the most important file in the engine.
//!
//! `create` is the single transaction the whole project exists to demonstrate:
//! order lock → validation → payment row → attempt row → (on success) ledger
//! postings → order transition → events → webhook jobs, all committing or
//! rolling back as one unit.
//!
//! # Lock order (see store::tx::LockOrder)
//!
//! `create` locks only the ORDER (rank 2) — the payment doesn't exist yet.
//! `capture` locks the PAYMENT (rank 1) first, then the ORDER (rank 2).
//! Ledger accounts are never locked; entries are inserts, balances are sums.
//!
//! # Why the decision is a parameter
//!
//! Engine deliberately does not depend on `simulator` (locked crate graph:
//! simulator is pure logic that `api` composes with engine). So `create` takes
//! an `AttemptDecision` in engine's own vocabulary; the api layer translates
//! `simulator::Decision` into it. This also makes every outcome path testable
//! here without a simulator in sight.

use std::collections::HashMap;
use std::sync::Arc;

use domain::clock::Clock;
use domain::id::{MerchantId, OrderId, PaymentId};
use domain::order::Order;
use domain::payment::{Capturable, Payment};
use domain::card::CardDetails;
use queue::job::kinds;
use serde_json::json;
use sqlx::PgPool;
use store::webhook_endpoint::WebhookEndpoint;
use store::StoreError;
use time::{Duration, OffsetDateTime};

use crate::error::{EngineError, EngineResult};
use crate::event;
use crate::fees::FeeSchedule;
use crate::ledger;

/// Engine's own outcome vocabulary. The api layer maps simulator::Decision
/// into this; tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Authorize and capture immediately.
    Capture,
    /// Capture now, then auto-open a dispute 60s later (the dispute test card).
    CaptureThenDispute,
    /// Authorize only — awaits an explicit capture call.
    Authorize,
    /// Declined with a code and customer-facing description.
    Decline { code: String, description: String },
    /// Needs a 3-D Secure challenge (flow lands in v2; payment waits).
    RequireAction,
    /// Held for risk review; a scheduled job resolves it later.
    RiskHold,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptDecision {
    pub outcome: AttemptOutcome,
    pub latency_ms: u32,
}

impl AttemptDecision {
    fn outcome_str(&self) -> &'static str {
        match self.outcome {
            AttemptOutcome::Capture | AttemptOutcome::CaptureThenDispute => "success",
            AttemptOutcome::Authorize => "authorized",
            AttemptOutcome::Decline { .. } => "declined",
            AttemptOutcome::RequireAction => "requires_action",
            AttemptOutcome::RiskHold => "risk_hold",
        }
    }

    fn decline_code(&self) -> Option<&str> {
        match &self.outcome {
            AttemptOutcome::Decline { code, .. } => Some(code),
            _ => None,
        }
    }
}

pub struct CreatePaymentInput {
    pub order_id: OrderId,
    /// Raw card number. Used transiently for validation, brand detection and
    /// fingerprinting, then dropped — it is never stored or logged.
    pub card_number: String,
    pub card_exp_month: u8,
    pub card_exp_year: u16,
    pub notes: HashMap<String, String>,
}

pub struct PaymentService {
    pool: PgPool,
    clock: Arc<dyn Clock>,
    fingerprint_pepper: String,
}

impl PaymentService {
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>, fingerprint_pepper: String) -> Self {
        Self {
            pool,
            clock,
            fingerprint_pepper,
        }
    }

    /// Create a payment attempt against an order and apply the decision.
    /// Everything inside one transaction.
    pub async fn create(
        &self,
        merchant_id: MerchantId,
        input: CreatePaymentInput,
        decision: AttemptDecision,
    ) -> EngineResult<Payment> {
        let now = self.clock.now();

        // Fan-out list, pre-fetched outside the transaction.
        let endpoints =
            store::webhook_endpoint::list_active_for_merchant(&self.pool, merchant_id).await?;

        let mut tx = store::tx::begin(&self.pool)
            .await
            .map_err(StoreError::from)?;

        // Rank-2 lock. Two concurrent attempts on one order serialize here.
        let mut order = store::order::find_by_id_for_update(&mut tx, input.order_id).await?;
        if order.merchant_id != merchant_id {
            return Err(EngineError::not_found("order"));
        }
        if order.status.is_terminal() {
            return Err(EngineError::validation(
                "order",
                "order is already paid or expired",
            ));
        }

        // Card metadata. The raw number's last use is inside these three lines.
        let fingerprint =
            crypto::fingerprint::card_fingerprint(&self.fingerprint_pepper, &input.card_number);
        let card = CardDetails::new(
            &input.card_number,
            fingerprint,
            input.card_exp_month,
            input.card_exp_year,
        )?;
        card.assert_not_expired(now)?;

        let mut payment =
            Payment::new_card_attempt(order.id, merchant_id, order.amount, card, input.notes, now);

        order.mark_attempted()?;
        store::order::update_status(&mut tx, order.id, order.status, order.amount_paid).await?;
        store::payment::insert(&mut tx, &payment).await?;
        store::payment_attempt::insert(
            &mut tx,
            payment.id,
            1,
            decision.outcome_str(),
            decision.decline_code(),
            decision.latency_ms as i32,
            now,
        )
        .await?;

        match &decision.outcome {
            AttemptOutcome::Capture => {
                self.settle(&mut tx, &mut payment, &mut order, &endpoints, now)
                    .await?;
            }
            AttemptOutcome::CaptureThenDispute => {
                self.settle(&mut tx, &mut payment, &mut order, &endpoints, now)
                    .await?;
                store::job::enqueue(
                    &mut tx,
                    kinds::OPEN_DISPUTE,
                    &json!({ "payment_id": payment.id.to_string() }),
                    now + Duration::seconds(60),
                    3,
                    now,
                )
                .await?;
            }
            AttemptOutcome::Authorize => {
                payment.mark_authorized()?;
                store::payment::update_status(
                    &mut tx,
                    payment.id,
                    payment.status,
                    None,
                    0,
                    None,
                    None,
                )
                .await?;
                event::emit(
                    &mut tx,
                    &endpoints,
                    merchant_id,
                    "payment.authorized",
                    event::payment_event_data(&payment, &order),
                    now,
                )
                .await?;
            }
            AttemptOutcome::Decline { code, description } => {
                payment.mark_failed(code, description)?;
                store::payment::update_status(
                    &mut tx,
                    payment.id,
                    payment.status,
                    None,
                    0,
                    Some(code),
                    Some(description),
                )
                .await?;
                event::emit(
                    &mut tx,
                    &endpoints,
                    merchant_id,
                    "payment.failed",
                    event::payment_event_data(&payment, &order),
                    now,
                )
                .await?;
            }
            AttemptOutcome::RequireAction => {
                payment.mark_requires_action()?;
                store::payment::update_status(
                    &mut tx,
                    payment.id,
                    payment.status,
                    None,
                    0,
                    None,
                    None,
                )
                .await?;
                // The 3DS challenge flow lands in v2; the payment waits here.
            }
            AttemptOutcome::RiskHold => {
                // Payment stays Created; a scheduled job resolves the review.
                // The handler registers at server wiring time.
                store::job::enqueue(
                    &mut tx,
                    "resolve_risk_review",
                    &json!({ "payment_id": payment.id.to_string() }),
                    now + Duration::seconds(30),
                    3,
                    now,
                )
                .await?;
            }
        }

        store::tx::commit(tx).await.map_err(StoreError::from)?;
        Ok(payment)
    }

    /// Manually capture an Authorized payment. Lock order: payment (1), order (2).
    pub async fn capture(
        &self,
        merchant_id: MerchantId,
        payment_id: PaymentId,
    ) -> EngineResult<Payment> {
        let now = self.clock.now();
        let endpoints =
            store::webhook_endpoint::list_active_for_merchant(&self.pool, merchant_id).await?;

        let mut tx = store::tx::begin(&self.pool)
            .await
            .map_err(StoreError::from)?;

        // Rank 1 first…
        let locked = store::payment::find_by_id_for_update(&mut tx, payment_id).await?;
        if locked.merchant_id != merchant_id {
            return Err(EngineError::not_found("payment"));
        }
        // The one runtime state check, at the boundary. Everything after this
        // point holds a compile-time proof the payment was capturable.
        let capturable = Capturable::try_from(locked)?;
        let mut payment = capturable.0;

        // …then rank 2.
        let mut order = store::order::find_by_id_for_update(&mut tx, payment.order_id).await?;

        self.settle(&mut tx, &mut payment, &mut order, &endpoints, now)
            .await?;

        store::tx::commit(tx).await.map_err(StoreError::from)?;
        Ok(payment)
    }

    pub async fn get(&self, merchant_id: MerchantId, id: PaymentId) -> EngineResult<Payment> {
        let payment = store::payment::find_by_id(&self.pool, id).await?;
        if payment.merchant_id != merchant_id {
            return Err(EngineError::not_found("payment"));
        }
        Ok(payment)
    }

    /// The shared success path: transition both aggregates, post the ledger,
    /// emit events. Caller has both rows locked.
    async fn settle(
        &self,
        tx: &mut store::tx::PgTx<'_>,
        payment: &mut Payment,
        order: &mut Order,
        endpoints: &[WebhookEndpoint],
        now: OffsetDateTime,
    ) -> EngineResult<()> {
        payment.mark_captured(now)?;
        order.mark_paid(payment.amount.minor())?;

        store::payment::update_status(
            tx,
            payment.id,
            payment.status,
            payment.captured_at,
            payment.amount_refunded,
            None,
            None,
        )
        .await?;
        store::order::update_status(tx, order.id, order.status, order.amount_paid).await?;

        let fees = FeeSchedule::for_currency(payment.amount.currency()).compute(payment.amount)?;
        ledger::post_capture(
            tx,
            payment.merchant_id,
            payment.amount,
            &fees,
            *payment.id.as_uuid(),
            now,
        )
        .await?;

        event::emit(
            tx,
            endpoints,
            payment.merchant_id,
            "payment.captured",
            event::payment_event_data(payment, order),
            now,
        )
        .await?;
        event::emit(
            tx,
            endpoints,
            payment.merchant_id,
            "order.paid",
            event::order_event_data(order),
            now,
        )
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{CreateOrderInput, OrderService};
    use domain::clock::FixedClock;
    use domain::ledger::account_type::AccountType;
    use domain::money::Currency;
    use domain::{order_status::OrderStatus, payment_status::PaymentStatus};
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    fn clock() -> Arc<FixedClock> {
        Arc::new(FixedClock::new(now()))
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        store::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    async fn seed_order(pool: &PgPool, mid: MerchantId) -> OrderId {
        let svc = OrderService::new(pool.clone(), clock());
        svc.create(
            mid,
            CreateOrderInput {
                amount_minor: 150_000,
                currency: Currency::Inr,
                receipt: Some("r_1".into()),
                notes: HashMap::new(),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn service(pool: &PgPool) -> PaymentService {
        PaymentService::new(pool.clone(), clock(), "test_pepper".into())
    }

    fn input(order_id: OrderId) -> CreatePaymentInput {
        CreatePaymentInput {
            order_id,
            card_number: "4242424242424242".into(),
            card_exp_month: 12,
            card_exp_year: 2030,
            notes: HashMap::new(),
        }
    }

    fn capture_decision() -> AttemptDecision {
        AttemptDecision {
            outcome: AttemptOutcome::Capture,
            latency_ms: 420,
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn success_path_captures_pays_the_order_and_balances_the_books(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let payment = svc
            .create(mid, input(oid), capture_decision())
            .await
            .unwrap();
        assert_eq!(payment.status, PaymentStatus::Captured);
        assert!(payment.captured_at.is_some());

        // Order is paid.
        let order = store::order::find_by_id(&pool, oid).await.unwrap();
        assert_eq!(order.status, OrderStatus::Paid);
        assert_eq!(order.amount_paid, 150_000);

        // Ledger: pending = amount - fee (3894 on 150000 INR).
        let balances = store::ledger::balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        let pending = balances
            .iter()
            .find(|b| b.account_type == AccountType::MerchantPending)
            .unwrap();
        assert_eq!(pending.balance.minor(), 146_106);

        // Events: payment.captured and order.paid both emitted.
        let events = store::event::list_for_merchant(&pool, mid, None, 10)
            .await
            .unwrap();
        let types: Vec<_> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"payment.captured"));
        assert!(types.contains(&"order.paid"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn decline_path_fails_the_payment_and_posts_no_ledger(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let decision = AttemptDecision {
            outcome: AttemptOutcome::Decline {
                code: "insufficient_funds".into(),
                description: "The card has insufficient funds.".into(),
            },
            latency_ms: 380,
        };
        let payment = svc.create(mid, input(oid), decision).await.unwrap();
        assert_eq!(payment.status, PaymentStatus::Failed);
        assert_eq!(payment.error_code.as_deref(), Some("insufficient_funds"));

        // Order attempted, not paid; retry remains possible.
        let order = store::order::find_by_id(&pool, oid).await.unwrap();
        assert_eq!(order.status, OrderStatus::Attempted);

        // No money moved.
        let balances = store::ledger::balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        assert!(balances.iter().all(|b| b.balance.is_zero()));

        let events = store::event::list_for_merchant(&pool, mid, None, 10)
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.event_type == "payment.failed"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn retry_after_decline_succeeds(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let decline = AttemptDecision {
            outcome: AttemptOutcome::Decline {
                code: "card_declined".into(),
                description: "The card was declined.".into(),
            },
            latency_ms: 300,
        };
        svc.create(mid, input(oid), decline).await.unwrap();
        let second = svc
            .create(mid, input(oid), capture_decision())
            .await
            .unwrap();
        assert_eq!(second.status, PaymentStatus::Captured);

        let order = store::order::find_by_id(&pool, oid).await.unwrap();
        assert_eq!(order.status, OrderStatus::Paid);

        // Two attempts exist against the order.
        let attempts = store::payment::list_for_order(&pool, oid).await.unwrap();
        assert_eq!(attempts.len(), 2);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn paying_a_paid_order_is_rejected(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        svc.create(mid, input(oid), capture_decision())
            .await
            .unwrap();
        let err = svc
            .create(mid, input(oid), capture_decision())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            EngineError::Validation { field: "order", .. }
        ));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn authorize_then_manual_capture(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let auth = AttemptDecision {
            outcome: AttemptOutcome::Authorize,
            latency_ms: 500,
        };
        let payment = svc.create(mid, input(oid), auth).await.unwrap();
        assert_eq!(payment.status, PaymentStatus::Authorized);

        // No money moved yet.
        let balances = store::ledger::balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        assert!(balances.iter().all(|b| b.balance.is_zero()));

        // Explicit capture settles everything.
        let captured = svc.capture(mid, payment.id).await.unwrap();
        assert_eq!(captured.status, PaymentStatus::Captured);

        let order = store::order::find_by_id(&pool, oid).await.unwrap();
        assert_eq!(order.status, OrderStatus::Paid);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn capturing_a_non_authorized_payment_is_rejected(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let payment = svc
            .create(mid, input(oid), capture_decision())
            .await
            .unwrap();
        // Already captured — capturing again must fail via Capturable.
        let err = svc.capture(mid, payment.id).await.unwrap_err();
        assert!(matches!(err, EngineError::PaymentTransition(_)));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn unknown_card_shape_is_rejected_before_any_write(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;
        let svc = service(&pool);

        let mut bad = input(oid);
        bad.card_number = "4242424242424241".into(); // fails Luhn
        let err = svc.create(mid, bad, capture_decision()).await.unwrap_err();
        assert!(matches!(err, EngineError::Card(_)));

        // Nothing was persisted — the transaction rolled back.
        let attempts = store::payment::list_for_order(&pool, oid).await.unwrap();
        assert!(attempts.is_empty());
        let order = store::order::find_by_id(&pool, oid).await.unwrap();
        assert_eq!(order.status, OrderStatus::Created);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn webhook_jobs_are_enqueued_when_an_endpoint_exists(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let oid = seed_order(&pool, mid).await;

        let ep = store::webhook_endpoint::WebhookEndpoint {
            id: domain::id::WebhookEndpointId::new(),
            merchant_id: mid,
            url: "https://shop.acme.dev/webhooks".into(),
            signing_secret: "whsec_x".into(),
            enabled_events: vec![],
            disabled_at: None,
            created_at: now(),
        };
        store::webhook_endpoint::insert(&pool, &ep).await.unwrap();

        service(&pool)
            .create(mid, input(oid), capture_decision())
            .await
            .unwrap();

        // payment.captured + order.paid → two deliveries, two jobs claimable.
        let mut tx = store::tx::begin(&pool).await.unwrap();
        let jobs = store::job::claim_batch(&mut tx, "test", 10, now())
            .await
            .unwrap();
        store::tx::commit(tx).await.unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.kind == "deliver_webhook"));
    }
}
