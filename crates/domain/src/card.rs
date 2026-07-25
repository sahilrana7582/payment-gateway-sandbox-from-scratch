use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardBrand {
    Visa,
    Mastercard,
    Amex,
    RuPay,
    Discover,
    Unknown,
}

impl CardBrand {
    pub fn detect(digits: &str) -> Self {
        if digits.starts_with('4') {
            return CardBrand::Visa;
        }

        if digits.len() >= 2 {
            let p2 = digits[0..2].parse::<u32>().unwrap_or(0);

            if (51..=55).contains(&p2) {
                return CardBrand::Mastercard;
            }

            if p2 == 34 || p2 == 37 {
                return CardBrand::Amex;
            }
            if p2 == 65 || p2 == 60 {
                return CardBrand::RuPay;
            }
        }

        if digits.len() >= 4 {
            let p4: u32 = digits[0..4].parse::<u32>().unwrap_or(0);
            if (2221..=2720).contains(&p4) {
                return CardBrand::Mastercard;
            }
        }

        CardBrand::Unknown
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            CardBrand::Visa => "visa",
            CardBrand::Mastercard => "mastercard",
            CardBrand::Amex => "amex",
            CardBrand::RuPay => "rupay",
            CardBrand::Discover => "discover",
            CardBrand::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for CardBrand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CardDetailsError {
    #[error("card number must be 12-19 digits, got {0}")]
    InvalidLength(usize),

    #[error("card number failed the Luhn check")]
    FailedLuhnCheck,

    #[error("expiry month must be 1-12, got {0}")]
    InvalidMonth(u8),

    #[error("card is expired")]
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardDetails {
    pub last4: String,
    pub brand: CardBrand,
    pub exp_month: u8,
    pub exp_year: u16,
    /// HMAC(pepper, PAN), computed by the `crypto` crate. Enables duplicate
    /// detection without retaining the number itself.
    pub fingerprint: String,
}

impl CardDetails {
    pub fn new(
        raw_number: &str,
        fingerprint: String,
        exp_month: u8,
        exp_year: u16,
    ) -> Result<Self, CardDetailsError> {
        let digits: String = raw_number.chars().filter(|c| c.is_ascii_digit()).collect();

        if digits.len() < 12 || digits.len() > 19 {
            return Err(CardDetailsError::InvalidLength(digits.len()));
        }

        if !luhn_valid(&digits) {
            return Err(CardDetailsError::FailedLuhnCheck);
        }

        if !(1..=12).contains(&exp_month) {
            return Err(CardDetailsError::InvalidMonth(exp_month));
        }

        let brand = CardBrand::detect(&digits);
        let last4 = digits[digits.len() - 4..].to_string();

        Ok(Self {
            last4,
            brand,
            exp_month,
            exp_year,
            fingerprint,
        })
    }

    pub fn assert_not_expired(&self, now: OffsetDateTime) -> Result<(), CardDetailsError> {
        let exp_year = i32::from(self.exp_year);
        let now_month = u8::from(now.month());
        if exp_year < now.year() || (exp_year == now.year() && self.exp_month < now_month) {
            return Err(CardDetailsError::Expired);
        }
        Ok(())
    }
}

pub fn luhn_valid(digits: &str) -> bool {
    let nums: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(10)).collect();
    if nums.len() < 2 {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for &d in nums.iter().rev() {
        let mut v = d;
        if double {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
        double = !double;
    }
    sum % 10 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const VISA_TEST: &str = "4242424242424242";
    const MASTERCARD_TEST: &str = "5555555555554444";

    #[test]
    fn valid_visa_card_constructs() {
        let card = CardDetails::new(VISA_TEST, "fp_abc".into(), 12, 2030).unwrap();
        assert_eq!(card.last4, "4242");
        assert_eq!(card.brand, CardBrand::Visa);
    }

    #[test]
    fn mastercard_detected() {
        let card = CardDetails::new(MASTERCARD_TEST, "fp_abc".into(), 12, 2030).unwrap();
        assert_eq!(card.brand, CardBrand::Mastercard);
    }

    #[test]
    fn fails_luhn_check() {
        let result = CardDetails::new("4242424242424241", "fp".into(), 12, 2030);
        assert_eq!(result, Err(CardDetailsError::FailedLuhnCheck));
    }

    #[test]
    fn rejects_bad_length() {
        let result = CardDetails::new("42424242", "fp".into(), 12, 2030);
        assert!(matches!(result, Err(CardDetailsError::InvalidLength(8))));
    }

    #[test]
    fn rejects_invalid_month() {
        let result = CardDetails::new(VISA_TEST, "fp".into(), 13, 2030);
        assert_eq!(result, Err(CardDetailsError::InvalidMonth(13)));
    }

    #[test]
    fn full_number_never_appears_in_last4() {
        let card = CardDetails::new(VISA_TEST, "fp".into(), 12, 2030).unwrap();
        assert_eq!(card.last4.len(), 4);
        assert!(!format!("{card:?}").contains("424242424242"));
    }

    #[test]
    fn expiry_check_accepts_future_date() {
        let card = CardDetails::new(VISA_TEST, "fp".into(), 6, 2030).unwrap();
        assert!(card
            .assert_not_expired(datetime!(2026-01-01 00:00:00 UTC))
            .is_ok());
    }

    #[test]
    fn expiry_check_rejects_past_year() {
        let card = CardDetails::new(VISA_TEST, "fp".into(), 6, 2020).unwrap();
        assert_eq!(
            card.assert_not_expired(datetime!(2026-01-01 00:00:00 UTC)),
            Err(CardDetailsError::Expired)
        );
    }

    #[test]
    fn expiry_check_rejects_earlier_month_same_year() {
        let card = CardDetails::new(VISA_TEST, "fp".into(), 3, 2026).unwrap();
        assert_eq!(
            card.assert_not_expired(datetime!(2026-06-01 00:00:00 UTC)),
            Err(CardDetailsError::Expired)
        );
    }

    #[test]
    fn expiry_check_accepts_exact_expiry_month() {
        let card = CardDetails::new(VISA_TEST, "fp".into(), 6, 2026).unwrap();
        assert!(card
            .assert_not_expired(datetime!(2026-06-15 00:00:00 UTC))
            .is_ok());
    }

    #[test]
    fn luhn_rejects_single_digit() {
        assert!(!luhn_valid("4"));
    }
}
