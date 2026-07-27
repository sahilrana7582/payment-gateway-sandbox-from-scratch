use serde::{Deserialize, Serialize};

use super::ledger_error::LedgerError;
use crate::id::LedgerAccountId;
use crate::money::{Currency, Money};

/// One side of a ledger transaction: an amount posted to one account.
///
/// Sign convention: **positive is a credit, negative is a debit.**
///
/// Rather than making callers remember that, use [`LedgerEntry::credit`] and
/// [`LedgerEntry::debit`], which both take a positive amount and apply the
/// correct sign. Sign errors in posting logic are subtle and produce books
/// that balance to zero while being completely wrong, so we remove the
/// opportunity to make them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub account_id: LedgerAccountId,
    pub amount: Money,
}

impl LedgerEntry {
    /// Credit an account: increases a liability or revenue account.
    /// `amount` must be positive.
    pub fn credit(account_id: LedgerAccountId, amount: Money) -> Result<Self, LedgerError> {
        if amount.is_zero() {
            return Err(LedgerError::ZeroAmountEntry);
        }
        if amount.is_negative() {
            return Err(LedgerError::Money(crate::money::MoneyError::Negative(
                amount.minor(),
            )));
        }
        Ok(Self { account_id, amount })
    }

    /// Debit an account: decreases a liability, or increases an asset.
    /// `amount` must be positive; it is stored negated.
    pub fn debit(account_id: LedgerAccountId, amount: Money) -> Result<Self, LedgerError> {
        if amount.is_zero() {
            return Err(LedgerError::ZeroAmountEntry);
        }
        if amount.is_negative() {
            return Err(LedgerError::Money(crate::money::MoneyError::Negative(
                amount.minor(),
            )));
        }
        Ok(Self {
            account_id,
            amount: amount.negate()?,
        })
    }

    /// Rehydrate from a database row, where the sign is already applied.
    /// Still rejects zero amounts — a zero entry is always a caller bug.
    pub fn from_signed(account_id: LedgerAccountId, amount: Money) -> Result<Self, LedgerError> {
        if amount.is_zero() {
            return Err(LedgerError::ZeroAmountEntry);
        }
        Ok(Self { account_id, amount })
    }

    pub const fn currency(&self) -> Currency {
        self.amount.currency()
    }

    pub const fn minor(&self) -> i64 {
        self.amount.minor()
    }

    pub const fn is_credit(&self) -> bool {
        self.amount.is_positive()
    }

    pub const fn is_debit(&self) -> bool {
        self.amount.is_negative()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> LedgerAccountId {
        LedgerAccountId::new()
    }

    #[test]
    fn credit_is_positive() {
        let e = LedgerEntry::credit(acct(), Money::new(1000, Currency::Inr)).unwrap();
        assert_eq!(e.minor(), 1000);
        assert!(e.is_credit());
        assert!(!e.is_debit());
    }

    #[test]
    fn debit_negates_the_amount() {
        let e = LedgerEntry::debit(acct(), Money::new(1000, Currency::Inr)).unwrap();
        assert_eq!(e.minor(), -1000);
        assert!(e.is_debit());
        assert!(!e.is_credit());
    }

    #[test]
    fn zero_amounts_are_rejected() {
        assert!(matches!(
            LedgerEntry::credit(acct(), Money::zero(Currency::Inr)),
            Err(LedgerError::ZeroAmountEntry)
        ));
        assert!(matches!(
            LedgerEntry::debit(acct(), Money::zero(Currency::Inr)),
            Err(LedgerError::ZeroAmountEntry)
        ));
    }

    #[test]
    fn constructors_reject_pre_negated_input() {
        // Callers must pass a positive magnitude; the constructor applies sign.
        assert!(LedgerEntry::credit(acct(), Money::new(-5, Currency::Inr)).is_err());
        assert!(LedgerEntry::debit(acct(), Money::new(-5, Currency::Inr)).is_err());
    }

    #[test]
    fn from_signed_preserves_sign_for_db_rehydration() {
        let e = LedgerEntry::from_signed(acct(), Money::new(-750, Currency::Usd)).unwrap();
        assert_eq!(e.minor(), -750);
        assert!(e.is_debit());
    }
}
