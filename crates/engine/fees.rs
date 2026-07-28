//! Fee calculation. All arithmetic in integer minor units — the rounding rule
//! is round-half-up (via `Money::percent`), documented here and nowhere else.
//!
//! Default schedule mirrors Indian gateway pricing:
//!   INR:    2% + ₹3.00, plus 18% GST on the fee
//!   others: 2% + 30 minor units, no tax

use domain::money::{Currency, Money, MoneyError};

/// A fee schedule. `percent_bps` is in basis points (200 = 2%) so schedules
/// like 2.9% are representable without floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSchedule {
    pub percent_bps: i64,
    pub fixed_minor: i64,
    /// Tax on the fee, as a whole percent (18 = 18% GST). Zero for currencies
    /// where we don't model tax.
    pub tax_percent: i64,
}

impl FeeSchedule {
    pub const fn for_currency(currency: Currency) -> Self {
        match currency {
            Currency::Inr => Self {
                percent_bps: 200,
                fixed_minor: 300,
                tax_percent: 18,
            },
            _ => Self {
                percent_bps: 200,
                fixed_minor: 30,
                tax_percent: 0,
            },
        }
    }

    /// Compute the breakdown for an amount. `base` is the platform's revenue,
    /// `tax` is owed to the tax authority, `total` is what the merchant pays.
    pub fn compute(&self, amount: Money) -> Result<FeeBreakdown, MoneyError> {
        let percent_part = amount.percent(self.percent_bps, 10_000)?;
        let base = percent_part
            .minor()
            .checked_add(self.fixed_minor)
            .ok_or(MoneyError::Overflow)?;

        let tax = Money::new(base, amount.currency())
            .percent(self.tax_percent, 100)?
            .minor();

        let total = base.checked_add(tax).ok_or(MoneyError::Overflow)?;

        Ok(FeeBreakdown {
            base,
            tax,
            total,
            currency: amount.currency(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub base: i64,
    pub tax: i64,
    pub total: i64,
    pub currency: Currency,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inr_fee_on_1500_rupees() {
        // 150000 paise: 2% = 3000, + 300 fixed = 3300 base.
        // GST 18% of 3300 = 594. Total 3894.
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(Money::new(150_000, Currency::Inr))
            .unwrap();
        assert_eq!(fees.base, 3300);
        assert_eq!(fees.tax, 594);
        assert_eq!(fees.total, 3894);
    }

    #[test]
    fn usd_fee_has_no_tax() {
        // 150000 cents: 2% = 3000, + 30 fixed = 3030 base. No tax.
        let fees = FeeSchedule::for_currency(Currency::Usd)
            .compute(Money::new(150_000, Currency::Usd))
            .unwrap();
        assert_eq!(fees.base, 3030);
        assert_eq!(fees.tax, 0);
        assert_eq!(fees.total, 3030);
    }

    #[test]
    fn rounding_is_half_up() {
        // 10001 paise at 2% = 200.02 → rounds to 200. +300 = 500 base.
        // 18% of 500 = 90. Total 590.
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(Money::new(10_001, Currency::Inr))
            .unwrap();
        assert_eq!(fees.base, 500);
        assert_eq!(fees.tax, 90);
        assert_eq!(fees.total, 590);

        // 10025 at 2% = 200.5 → half rounds UP to 201.
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(Money::new(10_025, Currency::Inr))
            .unwrap();
        assert_eq!(fees.base, 501);
    }

    #[test]
    fn fee_can_exceed_tiny_amounts() {
        // Minimum order is 100 minor units; the INR fee on it is 356 — larger
        // than the amount. That's allowed: the merchant's pending balance goes
        // negative, meaning they owe the platform. Real gateways handle this
        // with per-currency minimums; we document it instead.
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(Money::new(100, Currency::Inr))
            .unwrap();
        assert!(fees.total > 100);
    }
}
