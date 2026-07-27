use crate::ledger::account_type::AccountType;
// pub use entry::LedgerEntry;
// pub use transaction::{BalancedTransaction, LedgerTransactionKind};

use thiserror::Error;

use crate::money::{Currency, MoneyError};

/// Every way a ledger operation can fail. These are the invariants that
/// protect the books; each variant corresponds to an accounting rule.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LedgerError {
    /// A transaction must have at least two entries. A single-sided entry is
    /// not double-entry bookkeeping — it is a hole in the books.
    #[error("a ledger transaction requires at least 2 entries, got {0}")]
    TooFewEntries(usize),

    /// The core invariant: entries must sum to zero, per currency.
    /// The payload lists every currency that failed and by how much.
    #[error("transaction does not balance: {0:?}")]
    Unbalanced(Vec<(Currency, i64)>),

    /// A platform-level account (revenue, tax) must not be scoped to a merchant,
    /// and a merchant account must be.
    #[error("account type '{account_type}' has incorrect merchant scoping")]
    InvalidAccountScope { account_type: AccountType },

    /// An entry with a zero amount carries no information and is almost always
    /// a bug in the caller's posting logic.
    #[error("ledger entries must be non-zero")]
    ZeroAmountEntry,

    #[error("arithmetic overflow while summing entries")]
    Overflow,

    #[error(transparent)]
    Money(#[from] MoneyError),
}
