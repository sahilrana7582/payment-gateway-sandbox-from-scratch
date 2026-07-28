//! The magic test card table — the entire external banking world, replaced by
//! a lookup.
//!
//! # Two properties that matter
//!
//! **Determinism.** The same card always produces the same outcome. Real
//! issuers are nondeterministic; a sandbox must not be, or tutorials and graded
//! scenarios can't be reproducible.
//!
//! **Exhaustiveness as a safety control.** Any card number NOT in this table is
//! rejected. This is what makes it structurally impossible for a real card to
//! enter the system — no PCI scope, no live PAN in the database, no liability.
//! It is a security boundary, not a convenience.
//!
//! All numbers below satisfy the Luhn check, so client-side validation in the
//! checkout form behaves exactly as it would with a real card.

/// What the simulated acquirer/issuer decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Authorized and captured immediately (auto-capture merchants).
    Success,
    /// Authorized only — funds held, awaiting an explicit capture call.
    Authorize,
    /// Declined, with a specific reason.
    Declined(DeclineCode),
    /// Requires a 3-D Secure challenge before it can proceed.
    RequiresAction,
    /// Held for risk review, resolves after a short delay.
    RiskHold,
    /// Succeeds, then a dispute opens automatically 60 seconds later.
    SuccessThenDispute,
}

/// Decline reasons, mirroring the codes real gateways return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineCode {
    CardDeclined,
    InsufficientFunds,
    ExpiredCard,
    IncorrectCvc,
    ProcessingError,
    LostCard,
    StolenCard,
}

impl DeclineCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            DeclineCode::CardDeclined => "card_declined",
            DeclineCode::InsufficientFunds => "insufficient_funds",
            DeclineCode::ExpiredCard => "expired_card",
            DeclineCode::IncorrectCvc => "incorrect_cvc",
            DeclineCode::ProcessingError => "processing_error",
            DeclineCode::LostCard => "lost_card",
            DeclineCode::StolenCard => "stolen_card",
        }
    }

    /// Customer-facing message. Deliberately vague for lost/stolen — real
    /// gateways never tell the cardholder the card was reported stolen, since
    /// that tips off a thief.
    pub const fn description(self) -> &'static str {
        match self {
            DeclineCode::CardDeclined => "The card was declined.",
            DeclineCode::InsufficientFunds => "The card has insufficient funds.",
            DeclineCode::ExpiredCard => "The card has expired.",
            DeclineCode::IncorrectCvc => "The security code is incorrect.",
            DeclineCode::ProcessingError => "An error occurred while processing the card.",
            DeclineCode::LostCard => "The card was declined.",
            DeclineCode::StolenCard => "The card was declined.",
        }
    }
}

/// One entry in the published test card table.
pub struct TestCard {
    pub number: &'static str,
    pub outcome: Outcome,
    /// Shown in the docs table.
    pub description: &'static str,
}

/// The complete, published set. Everything else is rejected.
pub const TEST_CARDS: &[TestCard] = &[
    TestCard {
        number: "4242424242424242",
        outcome: Outcome::Success,
        description: "Succeeds and is captured immediately",
    },
    TestCard {
        number: "4000000000000077",
        outcome: Outcome::Authorize,
        description: "Authorizes only — requires a separate capture call",
    },
    TestCard {
        number: "4000000000000002",
        outcome: Outcome::Declined(DeclineCode::CardDeclined),
        description: "Declined — generic",
    },
    TestCard {
        number: "4000000000009995",
        outcome: Outcome::Declined(DeclineCode::InsufficientFunds),
        description: "Declined — insufficient funds",
    },
    TestCard {
        number: "4000000000000069",
        outcome: Outcome::Declined(DeclineCode::ExpiredCard),
        description: "Declined — expired card",
    },
    TestCard {
        number: "4000000000000127",
        outcome: Outcome::Declined(DeclineCode::IncorrectCvc),
        description: "Declined — incorrect CVC",
    },
    TestCard {
        number: "4000000000000119",
        outcome: Outcome::Declined(DeclineCode::ProcessingError),
        description: "Declined — processing error",
    },
    TestCard {
        number: "4000000000009987",
        outcome: Outcome::Declined(DeclineCode::LostCard),
        description: "Declined — reported lost",
    },
    TestCard {
        number: "4000000000009979",
        outcome: Outcome::Declined(DeclineCode::StolenCard),
        description: "Declined — reported stolen",
    },
    TestCard {
        number: "4000002760003184",
        outcome: Outcome::RequiresAction,
        description: "Requires 3-D Secure authentication",
    },
    TestCard {
        number: "4000000000000259",
        outcome: Outcome::SuccessThenDispute,
        description: "Succeeds, then a dispute opens after 60 seconds",
    },
    // Non-Visa brands, so brand detection can be exercised.
    TestCard {
        number: "5555555555554444",
        outcome: Outcome::Success,
        description: "Mastercard — succeeds",
    },
    TestCard {
        number: "6521000000000007",
        outcome: Outcome::Success,
        description: "RuPay — succeeds",
    },
    TestCard {
        number: "378282246310005",
        outcome: Outcome::Success,
        description: "Amex — succeeds",
    },
];

/// Normalize a card number: strip everything that isn't a digit.
pub fn normalize(card_number: &str) -> String {
    card_number.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Look up a card. `None` means the number is not in the test set and MUST be
/// rejected — this is the check that keeps real cards out.
pub fn lookup(card_number: &str) -> Option<&'static TestCard> {
    let digits = normalize(card_number);
    TEST_CARDS.iter().find(|c| c.number == digits)
}

/// Whether a number is in the published test set.
pub fn is_test_card(card_number: &str) -> bool {
    lookup(card_number).is_some()
}

#[cfg(test)]
mod tests {
    use domain::card::luhn_valid;

    use super::*;

    /// Every published card must pass Luhn, or client-side validation in the
    /// checkout form would reject it before it ever reached us.
    #[test]
    fn all_test_cards_pass_luhn() {
        for card in TEST_CARDS {
            assert!(luhn_valid(card.number), "{}", true);
        }
    }

    #[test]
    fn no_duplicate_card_numbers() {
        let mut seen = std::collections::HashSet::new();
        for card in TEST_CARDS {
            assert!(seen.insert(card.number), "duplicate: {}", card.number);
        }
    }

    #[test]
    fn lookup_finds_known_cards() {
        assert_eq!(
            lookup("4242424242424242").unwrap().outcome,
            Outcome::Success
        );
        assert_eq!(
            lookup("4000000000009995").unwrap().outcome,
            Outcome::Declined(DeclineCode::InsufficientFunds)
        );
    }

    #[test]
    fn lookup_normalizes_formatting() {
        assert!(lookup("4242 4242 4242 4242").is_some());
        assert!(lookup("4242-4242-4242-4242").is_some());
    }

    #[test]
    fn unknown_cards_are_rejected() {
        // A real, valid-Luhn card number that isn't in our table.
        assert!(lookup("4111111111111111").is_none());
        assert!(!is_test_card("4111111111111111"));
    }

    #[test]
    fn decline_codes_have_distinct_strings() {
        let codes = [
            DeclineCode::CardDeclined,
            DeclineCode::InsufficientFunds,
            DeclineCode::ExpiredCard,
            DeclineCode::IncorrectCvc,
            DeclineCode::ProcessingError,
            DeclineCode::LostCard,
            DeclineCode::StolenCard,
        ];
        let mut seen = std::collections::HashSet::new();
        for c in codes {
            assert!(seen.insert(c.as_str()));
        }
    }

    #[test]
    fn lost_and_stolen_do_not_leak_the_reason_to_the_customer() {
        // Security: never tell a cardholder the card was reported stolen.
        assert_eq!(
            DeclineCode::StolenCard.description(),
            DeclineCode::CardDeclined.description()
        );
        assert_eq!(
            DeclineCode::LostCard.description(),
            DeclineCode::CardDeclined.description()
        );
    }
}
