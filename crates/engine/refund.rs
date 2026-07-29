//! Refund orchestration.
//!
//! # Why the over-refund guard lives here and not in the database
//!
//! "The sum of a payment's refunds must not exceed the payment" is a cross-row
//! invariant, and a Postgres `CHECK` cannot query sibling rows. So it is
//! enforced in application code — but only ever *underneath a row lock on the
//! payment*. `store::refund::sum_for_payment` takes `&mut PgTx` for exactly
//! this reason: calling it without the lock held would let two concurrent
//! refunds each read the same total, each pass the check, and together exceed
//! the payment. The lock serialises them, so the second sees the first's row.
//!
//! # Why the fee reversal is computed cumulatively
//!
//! The obvious implementation — `fee_total * this_refund / payment_amount` —
//! drifts. Refund a ₹1500 payment in three ₹500 slices and rounding can strand
//! a paisa of fee on the platform's books forever. Instead we compute what the
//! *cumulative* fee owed should be at the new refunded total and subtract what
//! was already owed at the old total. When the payment is fully refunded the
//! cumulative figure is exactly `fee_total`, so the final slice always returns
//! the exact remainder. Self-correcting by construction.
//!
//! # Why refunds are instant here
//!
//! A real gateway creates a refund `pending` and settles it over days. The
//! sandbox marks it `processed` immediately: there is no bank to wait for, and
//! an integrator learns nothing from a delay whose end they cannot observe. The
//! `pending` state stays in the schema for the v2 settlement work.

use std::sync::Arc;

use domain::clock::Clock;
use domain::id::{MerchantId, PaymentId, RefundId};
use domain::money::Money;
use domain::payment_status::PaymentStatus;
use sqlx::PgPool;
use store::refund::{Refund, RefundStatus};
use store::StoreError;

use crate::error::{EngineError, EngineResult};
use crate::event;
use crate::fees::FeeSchedule;
use crate::ledger::{self, FeeRefund};

/// A refund reason is free text from the merchant; cap it so a pathological
/// value cannot bloat the row or the webhook payload.
const MAX_REASON_LENGTH: usize = 255;

#[derive(Debug, Clone)]
pub struct CreateRefundInput {
    pub payment_id: PaymentId,
    /// `None` refunds the entire remaining refundable amount. This is the
    /// common case, and it saves the caller from computing it — and from
    /// racing another refund while they do.
    pub amount_minor: Option<i64>,
    pub reason: Option<String>,
}

pub struct RefundService {
    pool: PgPool,
    clock: Arc<dyn Clock>,
}

impl RefundService {
    #[must_use]
    pub fn new(pool: PgPool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }

    /// Issue a full or partial refund against a captured payment.
    ///
    /// Lock order: PAYMENT only (rank 1). The order is deliberately not locked
    /// — a refund does not change it; it stays `paid`. Taking a lock we do not
    /// need would widen the deadlock surface for nothing.
    pub async fn create(
        &self,
        merchant_id: MerchantId,
        input: CreateRefundInput,
    ) -> EngineResult<Refund> {
        if let Some(reason) = &input.reason {
            if reason.len() > MAX_REASON_LENGTH {
                return Err(EngineError::validation(
                    "reason",
                    format!("reason must be at most {MAX_REASON_LENGTH} characters"),
                ));
            }
        }

        let now = self.clock.now();
        let endpoints =
            store::webhook_endpoint::list_active_for_merchant(&self.pool, merchant_id).await?;

        let mut tx = store::tx::begin(&self.pool)
            .await
            .map_err(StoreError::from)?;

        let mut payment = store::payment::find_by_id_for_update(&mut tx, input.payment_id).await?;
        if payment.merchant_id != merchant_id {
            return Err(EngineError::not_found("payment"));
        }

        // Only captured money can be returned. An authorized-but-uncaptured
        // payment is cancelled, not refunded; a failed one never moved money.
        if !matches!(
            payment.status,
            PaymentStatus::Captured | PaymentStatus::Refunded
        ) {
            return Err(EngineError::validation(
                "payment",
                format!(
                    "only a captured payment can be refunded (payment is '{}')",
                    payment.status
                ),
            ));
        }

        // Read under the lock. This is the guard.
        let already_refunded = store::refund::sum_for_payment(&mut tx, payment.id).await?;
        let remaining = payment.amount.minor() - already_refunded;

        if remaining <= 0 {
            return Err(EngineError::validation(
                "payment",
                "payment is already fully refunded",
            ));
        }

        let amount_minor = input.amount_minor.unwrap_or(remaining);
        if amount_minor <= 0 {
            return Err(EngineError::validation(
                "amount",
                "refund amount must be greater than zero",
            ));
        }
        if amount_minor > remaining {
            return Err(EngineError::validation(
                "amount",
                format!("refund amount exceeds the refundable balance of {remaining}"),
            ));
        }

        let currency = payment.amount.currency();
        let refund_amount = Money::new_non_negative(amount_minor, currency)?;
        let new_total = already_refunded + amount_minor;

        let refund = Refund {
            id: RefundId::new(),
            payment_id: payment.id,
            merchant_id,
            amount: refund_amount,
            reason: input.reason,
            status: RefundStatus::Processed,
            created_at: now,
            processed_at: Some(now),
        };
        store::refund::insert(&mut tx, &refund).await?;

        let fee_refund = proportional_fee_refund(payment.amount, already_refunded, new_total)?;
        ledger::post_refund(
            &mut tx,
            merchant_id,
            refund_amount,
            fee_refund,
            *refund.id.as_uuid(),
            now,
        )
        .await?;

        // A partial refund leaves the payment `captured` with a higher
        // `amount_refunded`; only a full one transitions to `refunded`. The
        // transition goes through the domain state machine so the legality
        // check is not duplicated here.
        if new_total >= payment.amount.minor() {
            payment.mark_refunded(new_total)?;
        } else {
            payment.amount_refunded = new_total;
        }
        store::payment::update_status(
            &mut tx,
            payment.id,
            payment.status,
            payment.captured_at,
            payment.amount_refunded,
            None,
            None,
        )
        .await?;

        event::emit(
            &mut tx,
            &endpoints,
            merchant_id,
            "refund.processed",
            event::refund_event_data(&refund, &payment),
            now,
        )
        .await?;

        store::tx::commit(tx).await.map_err(StoreError::from)?;
        Ok(refund)
    }

    pub async fn get(&self, merchant_id: MerchantId, id: RefundId) -> EngineResult<Refund> {
        let refund = store::refund::find_by_id(&self.pool, id).await?;
        if refund.merchant_id != merchant_id {
            return Err(EngineError::not_found("refund"));
        }
        Ok(refund)
    }

    /// All refunds against one payment. Ownership is checked on the payment
    /// first, so an unowned payment id yields "no such payment" rather than an
    /// empty list — an empty list would confirm the id exists.
    pub async fn list_for_payment(
        &self,
        merchant_id: MerchantId,
        payment_id: PaymentId,
    ) -> EngineResult<Vec<Refund>> {
        let payment = store::payment::find_by_id(&self.pool, payment_id).await?;
        if payment.merchant_id != merchant_id {
            return Err(EngineError::not_found("payment"));
        }
        Ok(store::refund::list_for_payment(&self.pool, payment_id).await?)
    }
}

/// Fee to return for the slice between `old_total` and `new_total` refunded.
///
/// Cumulative rather than per-slice, so rounding cannot strand a paisa: see the
/// module docs. The two components are computed independently because they land
/// in different ledger accounts.
fn proportional_fee_refund(
    payment_amount: Money,
    old_total: i64,
    new_total: i64,
) -> EngineResult<FeeRefund> {
    let fees = FeeSchedule::for_currency(payment_amount.currency()).compute(payment_amount)?;
    let amount = payment_amount.minor();
    let currency = payment_amount.currency();

    let owed = |component: i64, refunded: i64| -> EngineResult<i64> {
        if amount == 0 {
            return Ok(0);
        }
        Ok(Money::new(component, currency)
            .percent(refunded, amount)?
            .minor())
    };

    Ok(FeeRefund {
        base: owed(fees.base, new_total)? - owed(fees.base, old_total)?,
        tax: owed(fees.tax, new_total)? - owed(fees.tax, old_total)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::money::Currency;

    fn inr(minor: i64) -> Money {
        Money::new(minor, Currency::Inr)
    }

    #[test]
    fn a_full_refund_returns_the_entire_fee() {
        // 150000 INR -> base 3300, tax 594.
        let fee = proportional_fee_refund(inr(150_000), 0, 150_000).unwrap();
        assert_eq!(fee.base, 3300);
        assert_eq!(fee.tax, 594);
        assert_eq!(fee.total(), 3894);
    }

    #[test]
    fn a_half_refund_returns_half_the_fee() {
        let fee = proportional_fee_refund(inr(150_000), 0, 75_000).unwrap();
        assert_eq!(fee.base, 1650);
        assert_eq!(fee.tax, 297);
    }

    #[test]
    fn slicing_a_refund_returns_exactly_the_whole_fee_and_no_more() {
        // The property the cumulative approach exists for: three uneven slices
        // must sum to the full fee, with no paisa lost or invented.
        let amount = inr(150_000);
        let slices = [50_001i64, 49_999, 50_000];

        let mut running = 0i64;
        let mut base_returned = 0i64;
        let mut tax_returned = 0i64;
        for slice in slices {
            let fee = proportional_fee_refund(amount, running, running + slice).unwrap();
            base_returned += fee.base;
            tax_returned += fee.tax;
            running += slice;
        }

        assert_eq!(running, 150_000);
        assert_eq!(base_returned, 3300);
        assert_eq!(tax_returned, 594);
    }

    #[test]
    fn many_tiny_slices_still_sum_to_the_exact_fee() {
        let amount = inr(100_000);
        let mut running = 0i64;
        let mut total = 0i64;
        for _ in 0..100 {
            let fee = proportional_fee_refund(amount, running, running + 1_000).unwrap();
            total += fee.total();
            running += 1_000;
        }
        // 100000 INR -> base 2300, tax 414, total 2714.
        assert_eq!(running, 100_000);
        assert_eq!(total, 2714);
    }

    #[test]
    fn a_currency_without_tax_returns_no_tax_component() {
        let fee = proportional_fee_refund(Money::new(150_000, Currency::Usd), 0, 150_000).unwrap();
        assert_eq!(fee.tax, 0);
        assert!(fee.base > 0);
    }

    #[test]
    fn a_refund_small_enough_to_round_to_no_fee_is_zero_not_negative() {
        // One paisa of a large payment: the fee slice rounds to nothing. It
        // must never come out negative, which would credit the platform.
        let fee = proportional_fee_refund(inr(150_000), 0, 1).unwrap();
        assert!(fee.base >= 0);
        assert!(fee.tax >= 0);
    }
}
