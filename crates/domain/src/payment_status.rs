use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "db", derive(sqlx::Type))]
#[cfg_attr(
    feature = "db",
    sqlx(type_name = "payment_status", rename_all = "snake_case")
)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    /// Attempt created, not yet run through the acquirer.
    Created,
    /// Needs customer action — 3-D Secure challenge, redirect, etc.
    RequiresAction,
    /// Funds authorized/held, not yet captured. Only reachable in manual-capture flow.
    Authorized,
    /// Funds captured. Terminal for the happy path.
    Captured,
    /// Attempt failed — declined, expired action, capture failure. Terminal.
    Failed,
    /// Captured funds fully refunded. Terminal.
    Refunded,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("cannot transition payment from '{from:?}' to '{to:?}'")]
pub struct InvalidPaymentTransition {
    pub from: PaymentStatus,
    pub to: PaymentStatus,
}

impl PaymentStatus {
    pub fn transition(self, to: PaymentStatus) -> Result<PaymentStatus, InvalidPaymentTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidPaymentTransition { from: self, to })
        }
    }

    pub fn can_transition_to(self, to: PaymentStatus) -> bool {
        use PaymentStatus::*;
        matches!(
            (self, to),
            (Created, RequiresAction | Authorized | Captured | Failed)
                | (RequiresAction, Authorized | Captured | Failed)
                | (Authorized, Captured | Failed)
                | (Captured, Refunded)
        )
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            PaymentStatus::Captured | PaymentStatus::Failed | PaymentStatus::Refunded
        )
    }

    /// Only an Authorized payment can be captured via a separate capture call.
    /// (Created/RequiresAction going straight to Captured models auto-capture.)
    pub const fn is_capturable(self) -> bool {
        matches!(self, PaymentStatus::Authorized)
    }

    pub const fn all() -> &'static [PaymentStatus] {
        use PaymentStatus::*;
        &[
            Created,
            RequiresAction,
            Authorized,
            Captured,
            Failed,
            Refunded,
        ]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            PaymentStatus::Created => "created",
            PaymentStatus::RequiresAction => "requires_action",
            PaymentStatus::Authorized => "authorized",
            PaymentStatus::Captured => "captured",
            PaymentStatus::Failed => "failed",
            PaymentStatus::Refunded => "refunded",
        }
    }
}

impl std::fmt::Display for PaymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_auto_capture_path() {
        assert!(PaymentStatus::Created
            .transition(PaymentStatus::Captured)
            .is_ok());
    }

    #[test]
    fn payment_manual_capture_path() {
        assert!(PaymentStatus::Created
            .transition(PaymentStatus::Authorized)
            .is_ok());
        assert!(PaymentStatus::Authorized
            .transition(PaymentStatus::Captured)
            .is_ok());
    }

    #[test]
    fn payment_3ds_path() {
        assert!(PaymentStatus::Created
            .transition(PaymentStatus::RequiresAction)
            .is_ok());
        assert!(PaymentStatus::RequiresAction
            .transition(PaymentStatus::Captured)
            .is_ok());
    }

    #[test]
    fn payment_refund_only_from_captured() {
        assert!(PaymentStatus::Captured
            .transition(PaymentStatus::Refunded)
            .is_ok());
        assert!(PaymentStatus::Authorized
            .transition(PaymentStatus::Refunded)
            .is_err());
        assert!(PaymentStatus::Failed
            .transition(PaymentStatus::Refunded)
            .is_err());
    }

    #[test]
    fn payment_terminal_states_reject_everything() {
        for &terminal in &[
            PaymentStatus::Captured,
            PaymentStatus::Failed,
            PaymentStatus::Refunded,
        ] {
            for &to in PaymentStatus::all() {
                if to == terminal {
                    continue;
                }
                if terminal == PaymentStatus::Captured && to == PaymentStatus::Refunded {
                    continue; // the one allowed exit
                }
                assert!(
                    !terminal.can_transition_to(to),
                    "{terminal:?} -> {to:?} should be blocked"
                );
            }
        }
    }

    #[test]
    fn only_authorized_is_capturable() {
        assert!(PaymentStatus::Authorized.is_capturable());
        assert!(!PaymentStatus::Created.is_capturable());
        assert!(!PaymentStatus::Captured.is_capturable());
    }
}
