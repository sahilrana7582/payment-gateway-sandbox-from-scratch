//! Transaction helpers and — more importantly — the lock-ordering contract.
//!
//! # The deadlock rule
//!
//! Any code path that locks more than one row with `SELECT ... FOR UPDATE`
//! MUST take those locks in the order defined by [`LockOrder`]. Two
//! transactions that grab the same rows in opposite orders will deadlock:
//! txn A holds row 1 waiting for row 2, txn B holds row 2 waiting for row 1,
//! and Postgres kills one of them. Consistent ordering makes that impossible.
//!
//! The canonical hot path is capturing a payment, which touches: the payment
//! row, then the order row, then the merchant's ledger accounts. Always in
//! that sequence, and ledger accounts among themselves by ascending id.

use sqlx::{PgPool, Postgres, Transaction};

/// The transaction type threaded through the engine layer. Every multi-step
/// mutation (create payment + post ledger + emit event + enqueue job) runs on
/// one of these so it commits or rolls back as a single unit.
pub type PgTx<'a> = Transaction<'a, Postgres>;

/// Begin a transaction from the pool.
pub async fn begin(pool: &PgPool) -> Result<PgTx<'_>, sqlx::Error> {
    pool.begin().await
}

/// The mandated lock acquisition order. Lower number = locked first. This is
/// documentation enforced by convention and code review — Rust can't check it
/// for us, so it lives here as the single source of truth every service refers
/// to.
///
/// If you add a new lockable resource, give it a rank here and NEVER lock it
/// out of order relative to the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockOrder {
    Payment = 1,
    Order = 2,
    Refund = 3,
    Dispute = 4,
    /// Ledger accounts are locked last, and among themselves by ascending
    /// account id, so concurrent postings that touch overlapping accounts
    /// always agree on the sequence.
    LedgerAccount = 5,
}

/// Marker returned by helpers that commit, so a caller can't accidentally use a
/// transaction after committing it (the type is consumed by `commit`).
pub async fn commit(tx: PgTx<'_>) -> Result<(), sqlx::Error> {
    tx.commit().await
}

pub async fn rollback(tx: PgTx<'_>) -> Result<(), sqlx::Error> {
    tx.rollback().await
}
