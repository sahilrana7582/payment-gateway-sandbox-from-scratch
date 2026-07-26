use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::card::CardDetails;
use crate::id::{MerchantId, OrderId, PaymentId};
use crate::money::Money;
use crate::payment_method::PaymentMethodType;
use crate::payment_status::{InvalidPaymentTransition, PaymentStatus};

/// A single attempt to collect payment against an [`Order`](crate::order::Order).
/// An order can have several of these; normally exactly one ends up `Captured`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payment {
    pub id: PaymentId,
    pub order_id: OrderId,
    /// Denormalized from the order for authorization checks without a join.
    pub merchant_id: MerchantId,
    pub amount: Money,
    /// Cumulative refunded amount in minor units. `0` until a refund is issued.
    pub amount_refunded: i64,
    pub method: PaymentMethodType,
    pub card: Option<CardDetails>,
    pub status: PaymentStatus,
    pub error_code: Option<String>,
    pub error_description: Option<String>,
    pub notes: HashMap<String, String>,
    pub created_at: OffsetDateTime,
    pub captured_at: Option<OffsetDateTime>,
}

impl Payment {
    pub fn new_card_attempt(
        order_id: OrderId,
        merchant_id: MerchantId,
        amount: Money,
        card: CardDetails,
        notes: HashMap<String, String>,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            id: PaymentId::new(),
            order_id,
            merchant_id,
            amount,
            amount_refunded: 0,
            method: PaymentMethodType::Card,
            card: Some(card),
            status: PaymentStatus::Created,
            error_code: None,
            error_description: None,
            notes,
            created_at: now,
            captured_at: None,
        }
    }

    pub fn mark_requires_action(&mut self) -> Result<(), InvalidPaymentTransition> {
        self.status = self.status.transition(PaymentStatus::RequiresAction)?;
        Ok(())
    }

    pub fn mark_authorized(&mut self) -> Result<(), InvalidPaymentTransition> {
        self.status = self.status.transition(PaymentStatus::Authorized)?;
        Ok(())
    }

    pub fn mark_captured(&mut self, now: OffsetDateTime) -> Result<(), InvalidPaymentTransition> {
        self.status = self.status.transition(PaymentStatus::Captured)?;
        self.captured_at = Some(now);
        Ok(())
    }

    pub fn mark_failed(
        &mut self,
        code: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<(), InvalidPaymentTransition> {
        self.status = self.status.transition(PaymentStatus::Failed)?;
        self.error_code = Some(code.into());
        self.error_description = Some(description.into());
        Ok(())
    }

    pub fn mark_refunded(&mut self, amount_refunded: i64) -> Result<(), InvalidPaymentTransition> {
        self.status = self.status.transition(PaymentStatus::Refunded)?;
        self.amount_refunded = amount_refunded;
        Ok(())
    }

    pub fn amount_refundable(&self) -> i64 {
        (self.amount.minor() - self.amount_refunded).max(0)
    }

    pub fn is_fully_refunded(&self) -> bool {
        self.amount_refunded >= self.amount.minor()
    }
}

/// Proof that a [`Payment`] is in a capturable state (`Authorized`).
///
/// Obtained exactly once, at the service boundary, immediately after loading
/// the row under `SELECT ... FOR UPDATE`. Any function that needs to capture
/// a payment should accept `Capturable`, not `Payment` — that makes "capture
/// a payment that isn't authorized" a compile error at the call site rather
/// than a runtime check you might forget. The runtime check still happens,
/// exactly once, in `TryFrom`.
#[derive(Debug)]
pub struct Capturable(pub Payment);

impl TryFrom<Payment> for Capturable {
    type Error = InvalidPaymentTransition;

    fn try_from(payment: Payment) -> Result<Self, Self::Error> {
        if payment.status.is_capturable() {
            Ok(Capturable(payment))
        } else {
            Err(InvalidPaymentTransition {
                from: payment.status,
                to: PaymentStatus::Captured,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::CardBrand;
    use crate::money::Currency;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
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

    fn new_payment() -> Payment {
        Payment::new_card_attempt(
            OrderId::new(),
            MerchantId::new(),
            Money::new(150_000, Currency::Inr),
            card(),
            HashMap::new(),
            now(),
        )
    }

    #[test]
    fn new_payment_starts_created() {
        let p = new_payment();
        assert_eq!(p.status, PaymentStatus::Created);
        assert_eq!(p.amount_refunded, 0);
        assert_eq!(p.amount_refundable(), 150_000);
    }

    #[test]
    fn auto_capture_happy_path() {
        let mut p = new_payment();
        p.mark_captured(now()).unwrap();
        assert_eq!(p.status, PaymentStatus::Captured);
        assert_eq!(p.captured_at, Some(now()));
    }

    #[test]
    fn manual_capture_via_capturable_wrapper() {
        let mut p = new_payment();
        p.mark_authorized().unwrap();

        // Only an Authorized payment can become Capturable.
        let capturable = Capturable::try_from(p.clone()).unwrap();
        let mut inner = capturable.0;
        inner.mark_captured(now()).unwrap();
        assert_eq!(inner.status, PaymentStatus::Captured);
    }

    #[test]
    fn capturable_rejects_a_payment_still_in_created() {
        let p = new_payment(); // status: Created, not Authorized
        let result = Capturable::try_from(p);
        assert!(result.is_err());
    }

    #[test]
    fn capturable_rejects_an_already_captured_payment() {
        let mut p = new_payment();
        p.mark_captured(now()).unwrap();
        assert!(Capturable::try_from(p).is_err());
    }

    #[test]
    fn declined_payment_records_error_details() {
        let mut p = new_payment();
        p.mark_failed("card_declined", "The card was declined.")
            .unwrap();
        assert_eq!(p.status, PaymentStatus::Failed);
        assert_eq!(p.error_code.as_deref(), Some("card_declined"));
    }

    #[test]
    fn full_refund_updates_amount_and_status() {
        let mut p = new_payment();
        p.mark_captured(now()).unwrap();
        p.mark_refunded(150_000).unwrap();
        assert_eq!(p.status, PaymentStatus::Refunded);
        assert!(p.is_fully_refunded());
        assert_eq!(p.amount_refundable(), 0);
    }

    #[test]
    fn cannot_refund_before_capture() {
        let p = new_payment();
        // mark_refunded goes through PaymentStatus::transition, which only
        // allows Captured -> Refunded.
        let mut p = p;
        assert!(p.mark_refunded(150_000).is_err());
    }

    #[test]
    fn cannot_capture_a_failed_payment() {
        let mut p = new_payment();
        p.mark_failed("card_declined", "declined").unwrap();
        assert!(p.mark_captured(now()).is_err());
    }
}
