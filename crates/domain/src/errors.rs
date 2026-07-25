use thiserror::Error;

use crate::id::IdParseError;
use crate::money::MoneyError;
use crate::order_status::InvalidOrderTransition;
use crate::payment_status::InvalidPaymentTransition;

/// Every error the domain crate can produce, unified so callers in `engine`
/// can use `?` across module boundaries without hand-writing a From impl
/// for each one individually — `thiserror`'s `#[from]` generates those.
#[derive(Debug, Clone, Error)]
pub enum DomainError {
    #[error(transparent)]
    Money(#[from] MoneyError),

    #[error(transparent)]
    InvalidOrderTransition(#[from] InvalidOrderTransition),

    #[error(transparent)]
    InvalidPaymentTransition(#[from] InvalidPaymentTransition),

    #[error(transparent)]
    IdParse(#[from] IdParseError),

    #[error("validation failed: {field}: {reason}")]
    Validation { field: &'static str, reason: String },

    #[error("{resource} not found: {id}")]
    NotFound { resource: &'static str, id: String },
}

impl DomainError {
    pub fn validation(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Validation {
            field,
            reason: reason.into(),
        }
    }

    pub fn not_found(resource: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource,
            id: id.into(),
        }
    }
}
