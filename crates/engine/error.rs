use domain::{
    card::CardDetailsError, ledger::ledger_error::LedgerError, money::MoneyError,
    order_status::InvalidOrderTransition, payment_status::InvalidPaymentTransition,
};
use store::StoreError;
use thiserror::Error;

/// Every way an engine operation can fail. The api layer maps these to HTTP
/// status codes and the public error envelope; nothing below engine needs to
/// know about HTTP.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Money(#[from] MoneyError),

    #[error(transparent)]
    Ledger(#[from] LedgerError),

    #[error(transparent)]
    OrderTransition(#[from] InvalidOrderTransition),

    #[error(transparent)]
    PaymentTransition(#[from] InvalidPaymentTransition),

    #[error(transparent)]
    Card(#[from] CardDetailsError),

    #[error("validation failed: {field}: {reason}")]
    Validation { field: &'static str, reason: String },

    /// Also returned for cross-merchant access: asking for another merchant's
    /// order gets "not found", not "forbidden", so we never leak that the
    /// resource exists.
    #[error("{resource} not found")]
    NotFound { resource: &'static str },
}

impl EngineError {
    pub fn validation(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Validation {
            field,
            reason: reason.into(),
        }
    }

    pub fn not_found(resource: &'static str) -> Self {
        Self::NotFound { resource }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, EngineError::NotFound { .. })
            || matches!(self, EngineError::Store(s) if s.is_not_found())
    }
}

pub type EngineResult<T> = Result<T, EngineError>;
