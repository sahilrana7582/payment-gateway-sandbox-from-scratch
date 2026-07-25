use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::id::{MerchantId, OrderId};
use crate::money::Money;
use crate::order_status::{InvalidOrderTransition, OrderStatus};

/// What a merchant intends to collect. Separate from `Payment` — an order can
/// have zero, one, or several payment attempts against it, only one of which
/// (normally) ends up captured. This split is what makes reconciliation
/// teachable: "what we meant to collect" vs "the attempts to collect it".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub merchant_id: MerchantId,
    pub amount: Money,
    /// Cumulative amount actually captured against this order, in minor
    /// units. Zero until a payment captures.
    pub amount_paid: i64,
    /// The merchant's own reference — this is how their system finds the
    /// order again. Equivalent to Razorpay's `receipt`.
    pub receipt: Option<String>,
    /// Free-form merchant metadata, echoed back in webhooks untouched.
    pub notes: HashMap<String, String>,
    pub status: OrderStatus,
    pub created_at: OffsetDateTime,
}

impl Order {
    pub fn new(
        merchant_id: MerchantId,
        amount: Money,
        receipt: Option<String>,
        notes: HashMap<String, String>,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            id: OrderId::new(),
            merchant_id,
            amount,
            amount_paid: 0,
            receipt,
            notes,
            status: OrderStatus::Created,
            created_at: now,
        }
    }

    /// Call when a payment attempt is created against this order — including
    /// the second, third, etc. attempt after earlier ones failed. Legal from
    /// `Created` and from `Attempted` itself (retries).
    pub fn mark_attempted(&mut self) -> Result<(), InvalidOrderTransition> {
        self.status = self.status.transition(OrderStatus::Attempted)?;
        Ok(())
    }

    /// Call when a payment against this order captures. `amount_paid` is the
    /// captured amount, which the engine layer must have already validated
    /// against `self.amount` before calling this.
    pub fn mark_paid(&mut self, amount_paid: i64) -> Result<(), InvalidOrderTransition> {
        self.status = self.status.transition(OrderStatus::Paid)?;
        self.amount_paid = amount_paid;
        Ok(())
    }

    pub fn mark_expired(&mut self) -> Result<(), InvalidOrderTransition> {
        self.status = self.status.transition(OrderStatus::Expired)?;
        Ok(())
    }

    pub fn is_fully_paid(&self) -> bool {
        self.amount_paid >= self.amount.minor()
    }

    pub fn amount_due(&self) -> i64 {
        (self.amount.minor() - self.amount_paid).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    fn new_order() -> Order {
        Order::new(
            MerchantId::new(),
            Money::new(150_000, Currency::Inr),
            Some("internal_4471".into()),
            HashMap::new(),
            now(),
        )
    }

    #[test]
    fn new_order_starts_created_with_nothing_paid() {
        let o = new_order();
        assert_eq!(o.status, OrderStatus::Created);
        assert_eq!(o.amount_paid, 0);
        assert!(!o.is_fully_paid());
        assert_eq!(o.amount_due(), 150_000);
    }

    #[test]
    fn happy_path_to_paid() {
        let mut o = new_order();
        o.mark_attempted().unwrap();
        o.mark_paid(150_000).unwrap();
        assert_eq!(o.status, OrderStatus::Paid);
        assert!(o.is_fully_paid());
        assert_eq!(o.amount_due(), 0);
    }

    #[test]
    fn retry_after_failed_attempt_stays_attempted() {
        let mut o = new_order();
        o.mark_attempted().unwrap(); // first attempt
        assert_eq!(o.status, OrderStatus::Attempted);
        o.mark_attempted().unwrap(); // second attempt after first failed
        assert_eq!(o.status, OrderStatus::Attempted);
    }

    #[test]
    fn cannot_pay_a_paid_order_again() {
        let mut o = new_order();
        o.mark_attempted().unwrap();
        o.mark_paid(150_000).unwrap();
        assert!(o.mark_attempted().is_err());
        assert!(o.mark_paid(150_000).is_err());
    }

    #[test]
    fn expire_from_created_or_attempted() {
        let mut o1 = new_order();
        assert!(o1.mark_expired().is_ok());

        let mut o2 = new_order();
        o2.mark_attempted().unwrap();
        assert!(o2.mark_expired().is_ok());
    }

    #[test]
    fn amount_due_never_goes_negative() {
        let mut o = new_order();
        o.mark_attempted().unwrap();
        // Even if somehow overpaid, amount_due floors at zero rather than
        // reporting a negative "amount owed to the customer" figure here —
        // overpayment handling belongs to the engine layer, not this getter.
        o.amount_paid = 200_000;
        assert_eq!(o.amount_due(), 0);
    }
}
