use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{resource} not found")]
    NotFound { resource: &'static str },

    #[error("conflict: {constraint}")]
    Conflict { constraint: String },

    /// One of the ledger triggers from migration 0013 fired: the transaction
    /// didn't balance, or someone tried to mutate an immutable entry. This
    /// should be unreachable if the engine always posts via BalancedTransaction,
    /// but if it ever surfaces it means a real invariant was violated and the
    /// operation must abort loudly.
    #[error("ledger integrity violation: {0}")]
    LedgerIntegrity(String),

    /// A CHECK constraint other than the ledger triggers rejected the row.
    #[error("check constraint violated: {constraint}")]
    CheckViolation { constraint: String },

    /// Any other database error, passed through.
    #[error(transparent)]
    Database(sqlx::Error),

    /// A row was read whose stored value couldn't be mapped back into a domain
    /// type (e.g. an enum string not matching any variant). Indicates schema
    /// drift or manual tampering.
    #[error("failed to decode {what} from database: {detail}")]
    Decode { what: &'static str, detail: String },
}

impl StoreError {
    pub fn not_found(resource: &'static str) -> Self {
        Self::NotFound { resource }
    }

    pub fn decode(what: &'static str, detail: impl Into<String>) -> Self {
        Self::Decode {
            what,
            detail: detail.into(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::NotFound { .. })
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, StoreError::Conflict { .. })
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(err: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db_err) = &err {
            let code = db_err.code();
            let code = code.as_deref().unwrap_or("");
            let constraint = db_err.constraint().unwrap_or("").to_string();
            let message = db_err.message().to_string();

            match code {
                "23505" => {
                    return StoreError::Conflict {
                        constraint: if constraint.is_empty() {
                            message
                        } else {
                            constraint
                        },
                    };
                }
                "23514" => {
                    return StoreError::CheckViolation {
                        constraint: if constraint.is_empty() {
                            message
                        } else {
                            constraint
                        },
                    };
                }
                // Our ledger triggers raise 23001 (integrity_constraint_violation).
                "23001" => {
                    return StoreError::LedgerIntegrity(message);
                }

                _ => {}
            }
        }

        StoreError::Database(err)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;
