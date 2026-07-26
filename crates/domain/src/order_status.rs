use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "db", derive(sqlx::Type))]
#[cfg_attr(feature = "db", sqlx(type_name = "order_status", rename_all = "snake_case"))]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Order created, no payment attempted yet.
    Created,
    /// At least one payment attempt exists, none captured yet.
    Attempted,
    /// A payment captured. Terminal for the happy path.
    Paid,
    /// Order expired before payment completed. Terminal.
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("cannot transition order from '{from:?}' to '{to:?}'")]
pub struct InvalidOrderTransition {
    pub from: OrderStatus,
    pub to: OrderStatus,
}

impl OrderStatus {
    pub fn transition(self, to: OrderStatus) -> Result<OrderStatus, InvalidOrderTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidOrderTransition { from: self, to })
        }
    }

    pub fn can_transition_to(self, to: OrderStatus) -> bool {
        use OrderStatus::*;
        matches!(
            (self, to),
            (Created, Attempted | Expired) | (Attempted, Attempted | Paid | Expired)
        )
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, OrderStatus::Paid | OrderStatus::Expired)
    }

    pub const fn all() -> &'static [OrderStatus] {
        use OrderStatus::*;
        &[Created, Attempted, Paid, Expired]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Created => "created",
            OrderStatus::Attempted => "attempted",
            OrderStatus::Paid => "paid",
            OrderStatus::Expired => "expired",
        }
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_happy_path() {
        assert!(OrderStatus::Created
            .transition(OrderStatus::Attempted)
            .is_ok());
        assert!(OrderStatus::Attempted.transition(OrderStatus::Paid).is_ok());
    }

    #[test]
    fn order_can_retry_after_a_failed_attempt() {
        // Attempted -> Attempted models "first attempt failed, second attempt made"
        assert!(OrderStatus::Attempted
            .transition(OrderStatus::Attempted)
            .is_ok());
    }

    #[test]
    fn order_terminal_states_reject_everything() {
        for &terminal in &[OrderStatus::Paid, OrderStatus::Expired] {
            for &to in OrderStatus::all() {
                if to == terminal {
                    continue;
                }
                assert!(!terminal.can_transition_to(to));
            }
        }
    }
}
