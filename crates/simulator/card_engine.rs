//! The decision engine: card + amount → outcome.
//!
//! This function is where the entire external banking world lives. In a real
//! gateway this is 800ms of ISO 8583 messages across an acquirer, a card
//! network, and an issuing bank. Here it's a table lookup plus an amount rule —
//! deterministic, instant, and reproducible.
//!
//! We add *simulated* latency on top so integrators don't build UIs that assume
//! payments resolve instantly.

use domain::money::Money;
use thiserror::Error;

use crate::cards::{self, DeclineCode, Outcome};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SimulatorError {
    /// The card is not in the published test set. This is the control that
    /// keeps real card numbers out of the system entirely.
    #[error("card number is not a recognized test card")]
    NotATestCard,
}

/// The simulator's full verdict on one payment attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub outcome: Outcome,
    /// Simulated round-trip time, recorded on the payment attempt so the
    /// dashboard timeline shows realistic numbers.
    pub latency_ms: u32,
}

impl Decision {
    pub fn is_success(&self) -> bool {
        matches!(self.outcome, Outcome::Success | Outcome::SuccessThenDispute)
    }

    pub fn decline_code(&self) -> Option<DeclineCode> {
        match self.outcome {
            Outcome::Declined(code) => Some(code),
            _ => None,
        }
    }

    /// String form for `payment_attempts.outcome`.
    pub fn outcome_str(&self) -> &'static str {
        match self.outcome {
            Outcome::Success | Outcome::SuccessThenDispute => "success",
            Outcome::Authorize => "authorized",
            Outcome::Declined(_) => "declined",
            Outcome::RequiresAction => "requires_action",
            Outcome::RiskHold => "risk_hold",
        }
    }
}

/// Amount-based override: any amount whose last two minor-unit digits are `05`
/// goes to risk review, regardless of card. Gives integrators a way to trigger
/// the review path without needing a dedicated card, and mirrors how real
/// fraud engines key partly on amount.
fn amount_override(amount: Money) -> Option<Outcome> {
    if amount.minor() % 100 == 5 {
        return Some(Outcome::RiskHold);
    }
    None
}

/// Deterministic pseudo-latency derived from the card number, so the same card
/// always reports the same timing — reproducibility again. Range is roughly
/// 300–800ms, matching real card authorization.
fn simulated_latency(digits: &str) -> u32 {
    let sum: u32 = digits.bytes().map(|b| b as u32).sum();
    300 + (sum % 500)
}

/// Decide the fate of a payment attempt.
///
/// Order of precedence: the card must be a known test card FIRST (the safety
/// gate), then amount overrides, then the card's own configured outcome.
pub fn decide(card_number: &str, amount: Money) -> Result<Decision, SimulatorError> {
    let card = cards::lookup(card_number).ok_or(SimulatorError::NotATestCard)?;
    let digits = cards::normalize(card_number);

    // Amount override wins over the card's default outcome, but only for cards
    // that would otherwise succeed — a declined card stays declined regardless
    // of amount, which matches real issuer behavior.
    let outcome = match amount_override(amount) {
        Some(override_outcome) if matches!(card.outcome, Outcome::Success) => override_outcome,
        _ => card.outcome,
    };

    Ok(Decision {
        outcome,
        latency_ms: simulated_latency(&digits),
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
    fn success_card_succeeds() {
        let d = decide("4242424242424242", inr(150_000)).unwrap();
        assert_eq!(d.outcome, Outcome::Success);
        assert!(d.is_success());
    }

    #[test]
    fn declined_card_declines_with_the_right_code() {
        let d = decide("4000000000009995", inr(150_000)).unwrap();
        assert_eq!(d.decline_code(), Some(DeclineCode::InsufficientFunds));
        assert!(!d.is_success());
    }

    #[test]
    fn unknown_card_is_rejected_outright() {
        // The safety gate: a real card number must never get a decision.
        let err = decide("4111111111111111", inr(150_000)).unwrap_err();
        assert_eq!(err, SimulatorError::NotATestCard);
    }

    #[test]
    fn decisions_are_deterministic() {
        let a = decide("4242424242424242", inr(150_000)).unwrap();
        let b = decide("4242424242424242", inr(150_000)).unwrap();
        assert_eq!(a, b); // same outcome AND same latency
    }

    #[test]
    fn amount_ending_in_05_triggers_risk_hold() {
        let d = decide("4242424242424242", inr(150_005)).unwrap();
        assert_eq!(d.outcome, Outcome::RiskHold);
    }

    #[test]
    fn amount_override_does_not_rescue_a_declined_card() {
        // A declined card stays declined even at a risk-hold amount.
        let d = decide("4000000000000002", inr(150_005)).unwrap();
        assert_eq!(d.outcome, Outcome::Declined(DeclineCode::CardDeclined));
    }

    #[test]
    fn three_ds_card_requires_action() {
        let d = decide("4000002760003184", inr(150_000)).unwrap();
        assert_eq!(d.outcome, Outcome::RequiresAction);
    }

    #[test]
    fn authorize_only_card_does_not_capture() {
        let d = decide("4000000000000077", inr(150_000)).unwrap();
        assert_eq!(d.outcome, Outcome::Authorize);
        assert!(!d.is_success()); // authorized is not yet captured
    }

    #[test]
    fn dispute_card_succeeds_first() {
        let d = decide("4000000000000259", inr(150_000)).unwrap();
        assert_eq!(d.outcome, Outcome::SuccessThenDispute);
        assert!(d.is_success());
    }

    #[test]
    fn latency_is_in_a_realistic_range() {
        for card in cards::TEST_CARDS {
            let d = decide(card.number, inr(150_000)).unwrap();
            assert!(
                (300..=800).contains(&d.latency_ms),
                "{} latency {} out of range",
                card.number,
                d.latency_ms
            );
        }
    }

    #[test]
    fn formatting_does_not_change_the_decision() {
        let plain = decide("4242424242424242", inr(1000)).unwrap();
        let spaced = decide("4242 4242 4242 4242", inr(1000)).unwrap();
        assert_eq!(plain, spaced);
    }
}
