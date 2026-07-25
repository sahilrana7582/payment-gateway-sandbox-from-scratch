use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Currency {
    Inr,
    Usd,
    Eur,
    Jpy,
    Kwd,
}

impl Currency {
    /// Number of decimal places. Exhaustive match — adding a new currency
    /// variant that doesn't have an exponent here will not compile.
    pub const fn exponent(self) -> u32 {
        match self {
            Currency::Inr | Currency::Usd | Currency::Eur => 2,
            Currency::Jpy => 0,
            Currency::Kwd => 3,
        }
    }

    pub const fn code(self) -> &'static str {
        match self {
            Currency::Inr => "INR",
            Currency::Usd => "USD",
            Currency::Eur => "EUR",
            Currency::Jpy => "JPY",
            Currency::Kwd => "KWD",
        }
    }
}

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::str::FromStr for Currency {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "INR" => Ok(Currency::Inr),
            "USD" => Ok(Currency::Usd),
            "EUR" => Ok(Currency::Eur),
            "JPY" => Ok(Currency::Jpy),
            "KWD" => Ok(Currency::Kwd),
            other => Err(MoneyError::UnknownCurrency(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    #[error("currency mismatch: cannot combine {lhs} and {rhs}")]
    CurrencyMismatch { lhs: Currency, rhs: Currency },

    #[error("arithmetic overflow")]
    Overflow,

    #[error("amount must not be negative, got {0}")]
    Negative(i64),

    #[error("unknown currency: {0}")]
    UnknownCurrency(String),
}

/// An amount of money in a specific currency, stored as an integer count of
/// the smallest indivisible unit (minor units: paise, cents, fils).
///
/// # Why no `impl Add`
///
/// The `+` operator cannot return a `Result`. If we implemented it, adding
/// INR to USD would either panic or silently produce garbage. Callers must
/// use `.add()` and explicitly handle the error. In a ledger, that discipline
/// is the difference between books that balance and books that don't.
///
/// # Why `i64`, not `u64`
///
/// Ledger entries are signed. A debit posted to an account is a negative
/// value. Using `i64` throughout means we never need a separate type for
/// credits and debits.
///
/// # Why not `f64`
///
/// `0.1 + 0.2 != 0.3` in floating point. That discrepancy is literally lost
/// money. Never use float for monetary values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    minor: i64,
    currency: Currency,
}

impl Money {
    /// Construct without constraints. Suitable for ledger entries, which can
    /// be negative (debits).
    pub const fn new(minor: i64, currency: Currency) -> Self {
        Self { minor, currency }
    }

    /// Construct with a non-negativity check. Use at API boundaries where a
    /// customer-facing amount must be positive (charges, refund amounts, etc.).
    pub fn new_non_negative(minor: i64, currency: Currency) -> Result<Self, MoneyError> {
        if minor < 0 {
            return Err(MoneyError::Negative(minor));
        }
        Ok(Self { minor, currency })
    }

    /// Additive identity for a given currency. Useful as a fold seed.
    pub const fn zero(currency: Currency) -> Self {
        Self { minor: 0, currency }
    }

    pub const fn minor(self) -> i64 {
        self.minor
    }

    pub const fn currency(self) -> Currency {
        self.currency
    }

    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    pub const fn is_negative(self) -> bool {
        self.minor < 0
    }

    pub const fn is_positive(self) -> bool {
        self.minor > 0
    }

    /// Checked addition. Fails on currency mismatch or `i64` overflow.
    #[allow(clippy::should_implement_trait)] // returns Result, not std::ops::Add
    pub fn add(self, other: Self) -> Result<Self, MoneyError> {
        self.check_same_currency(other)?;
        let minor = self
            .minor
            .checked_add(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Self {
            minor,
            currency: self.currency,
        })
    }

    /// Checked subtraction. Result may be negative (ledger use case).
    #[allow(clippy::should_implement_trait)] // returns Result, not std::ops::Sub
    pub fn sub(self, other: Self) -> Result<Self, MoneyError> {
        self.check_same_currency(other)?;
        let minor = self
            .minor
            .checked_sub(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Self {
            minor,
            currency: self.currency,
        })
    }

    /// Integer percentage: `self * numerator / denominator`.
    /// Uses i128 internally to avoid overflow during the multiplication step.
    /// Rounds half-up (standard financial rounding).
    pub fn percent(self, numerator: i64, denominator: i64) -> Result<Self, MoneyError> {
        if denominator == 0 {
            return Err(MoneyError::Overflow);
        }
        let numer = self.minor as i128 * numerator as i128;
        let denom = denominator as i128;
        // Round half-up: add denom/2 before dividing (for positive values).
        let rounded = if numer >= 0 {
            (numer + denom / 2) / denom
        } else {
            (numer - denom / 2) / denom
        };
        let minor = i64::try_from(rounded).map_err(|_| MoneyError::Overflow)?;
        Ok(Self {
            minor,
            currency: self.currency,
        })
    }

    /// Negate. Turns a credit into a debit and vice versa.
    pub fn negate(self) -> Result<Self, MoneyError> {
        let minor = self.minor.checked_neg().ok_or(MoneyError::Overflow)?;
        Ok(Self {
            minor,
            currency: self.currency,
        })
    }

    /// Human-readable display. For example:
    /// - `Money::new(150000, Currency::Inr)` → `"1500.00 INR"`
    /// - `Money::new(150000, Currency::Jpy)` → `"150000 JPY"`
    /// - `Money::new(1500000, Currency::Kwd)` → `"1500.000 KWD"`
    ///
    /// This is for display only. The database always stores the raw `minor` integer.
    pub fn format(self) -> String {
        let exp = self.currency.exponent();
        if exp == 0 {
            return format!("{} {}", self.minor, self.currency.code());
        }
        let divisor = 10i64.pow(exp);
        let major = self.minor / divisor;
        let frac = (self.minor % divisor).abs();
        let sign = if self.minor < 0 && major == 0 {
            "-"
        } else {
            ""
        };
        format!(
            "{sign}{major}.{frac:0>width$} {code}",
            width = exp as usize,
            code = self.currency.code(),
        )
    }

    fn check_same_currency(self, other: Self) -> Result<(), MoneyError> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch {
                lhs: self.currency,
                rhs: other.currency,
            });
        }
        Ok(())
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_currency_adds() {
        let a = Money::new(100, Currency::Inr);
        let b = Money::new(250, Currency::Inr);
        assert_eq!(a.add(b).unwrap(), Money::new(350, Currency::Inr));
    }

    #[test]
    fn cross_currency_is_a_typed_error_not_a_panic() {
        let inr = Money::new(100, Currency::Inr);
        let usd = Money::new(100, Currency::Usd);
        assert!(matches!(
            inr.add(usd),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn overflow_is_caught() {
        let a = Money::new(i64::MAX, Currency::Usd);
        let b = Money::new(1, Currency::Usd);
        assert!(matches!(a.add(b), Err(MoneyError::Overflow)));
    }

    #[test]
    fn negative_amount_rejected_at_non_negative_constructor() {
        assert!(matches!(
            Money::new_non_negative(-1, Currency::Eur),
            Err(MoneyError::Negative(-1))
        ));
        assert!(Money::new_non_negative(0, Currency::Eur).is_ok());
        assert!(Money::new_non_negative(1, Currency::Eur).is_ok());
    }

    #[test]
    fn subtraction_can_produce_negative_for_ledger_use() {
        let a = Money::new(100, Currency::Usd);
        let b = Money::new(250, Currency::Usd);
        assert_eq!(a.sub(b).unwrap(), Money::new(-150, Currency::Usd));
    }

    #[test]
    fn percent_rounds_half_up() {
        // 2% of 10001 paise = 200.02 → rounds to 200
        let amount = Money::new(10001, Currency::Inr);
        let fee = amount.percent(2, 100).unwrap();
        assert_eq!(fee.minor(), 200);
    }

    #[test]
    fn format_respects_currency_exponent() {
        assert_eq!(Money::new(150000, Currency::Inr).format(), "1500.00 INR");
        assert_eq!(Money::new(150000, Currency::Jpy).format(), "150000 JPY");
        assert_eq!(Money::new(1500000, Currency::Kwd).format(), "1500.000 KWD");
    }

    #[test]
    fn format_negative_fractional() {
        // -50 paise = -0.50 INR
        assert_eq!(Money::new(-50, Currency::Inr).format(), "-0.50 INR");
    }

    #[test]
    fn negate() {
        let a = Money::new(500, Currency::Usd);
        assert_eq!(a.negate().unwrap(), Money::new(-500, Currency::Usd));
    }

    #[test]
    fn zero_is_zero() {
        let z = Money::zero(Currency::Inr);
        assert!(z.is_zero());
        assert!(!z.is_negative());
        assert!(!z.is_positive());
    }

    #[test]
    fn currency_roundtrips_through_string() {
        for code in ["INR", "USD", "EUR", "JPY", "KWD"] {
            let c: Currency = code.parse().unwrap();
            assert_eq!(c.code(), code);
        }
    }
}
