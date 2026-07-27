use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{ledger_entry::LedgerEntry, ledger_error::LedgerError};
use crate::money::Currency;

/// Why a ledger transaction was posted. Stored alongside the entries so the
/// dashboard and any audit can explain every movement of money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerTransactionKind {
    /// A payment was captured: funds move from clearing into merchant pending.
    PaymentCaptured,
    /// The platform's processing fee and any tax on it.
    FeeCharged,
    /// A refund was issued back to the cardholder.
    RefundIssued,
    /// The proportional fee returned to the merchant on a refund.
    FeeRefunded,
    /// Funds frozen because a dispute was opened.
    DisputeOpened,
    /// Dispute resolved in the merchant's favour: frozen funds released.
    DisputeWon,
    /// Dispute resolved against the merchant: frozen funds forfeited.
    DisputeLost,
    /// Settlement window elapsed: pending funds become available.
    FundsReleased,
    /// A payout was made to the merchant's bank.
    PayoutPaid,
    /// Funds withheld into the risk reserve.
    ReserveHeld,
    /// Funds released from the risk reserve.
    ReserveReleased,
}

impl LedgerTransactionKind {
    pub const fn as_str(self) -> &'static str {
        use LedgerTransactionKind::*;
        match self {
            PaymentCaptured => "payment_captured",
            FeeCharged => "fee_charged",
            RefundIssued => "refund_issued",
            FeeRefunded => "fee_refunded",
            DisputeOpened => "dispute_opened",
            DisputeWon => "dispute_won",
            DisputeLost => "dispute_lost",
            FundsReleased => "funds_released",
            PayoutPaid => "payout_paid",
            ReserveHeld => "reserve_held",
            ReserveReleased => "reserve_released",
        }
    }
}

impl std::fmt::Display for LedgerTransactionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of ledger entries that provably sum to zero in every currency.
///
/// # The point of this type
///
/// `LedgerService::post()` accepts **only** a `BalancedTransaction`. Because
/// the only way to obtain one is through [`BalancedTransaction::new`], which
/// validates the sum, it is impossible to write an unbalanced transaction to
/// the database through the normal API. The invariant is enforced by the type
/// system rather than by remembering to call a validate function.
///
/// The database has a deferred constraint trigger enforcing the same rule.
/// Two independent layers, so a bug in one is caught by the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalancedTransaction {
    kind: LedgerTransactionKind,
    entries: Vec<LedgerEntry>,
}

impl BalancedTransaction {
    /// Validate and construct. Fails if:
    /// - there are fewer than two entries (not double-entry),
    /// - any currency's entries do not sum to exactly zero,
    /// - summing overflows `i64`.
    pub fn new(
        kind: LedgerTransactionKind,
        entries: Vec<LedgerEntry>,
    ) -> Result<Self, LedgerError> {
        if entries.len() < 2 {
            return Err(LedgerError::TooFewEntries(entries.len()));
        }

        let mut sums: HashMap<Currency, i64> = HashMap::new();
        for entry in &entries {
            let slot = sums.entry(entry.currency()).or_insert(0);
            *slot = slot
                .checked_add(entry.minor())
                .ok_or(LedgerError::Overflow)?;
        }

        let mut unbalanced: Vec<(Currency, i64)> =
            sums.into_iter().filter(|&(_, total)| total != 0).collect();

        if !unbalanced.is_empty() {
            // Deterministic ordering makes the error message stable in tests.
            unbalanced.sort_by_key(|(c, _)| c.code());
            return Err(LedgerError::Unbalanced(unbalanced));
        }

        Ok(Self { kind, entries })
    }

    pub fn kind(&self) -> LedgerTransactionKind {
        self.kind
    }

    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<LedgerEntry> {
        self.entries
    }

    /// Every currency touched by this transaction.
    pub fn currencies(&self) -> Vec<Currency> {
        let mut seen: Vec<Currency> = Vec::new();
        for e in &self.entries {
            if !seen.contains(&e.currency()) {
                seen.push(e.currency());
            }
        }
        seen
    }

    /// Total credited (positive side) in a given currency. Equals the total
    /// debited, by construction — useful for display and for assertions.
    pub fn total_credited(&self, currency: Currency) -> i64 {
        self.entries
            .iter()
            .filter(|e| e.currency() == currency && e.is_credit())
            .map(|e| e.minor())
            .sum()
    }
}

/// Ergonomic construction. Collect entries, then `build()` to validate.
///
/// ```ignore
/// let txn = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
///     .debit(clearing_account, amount)?
///     .credit(merchant_pending, amount)?
///     .build()?;
/// ```
pub struct TransactionBuilder {
    kind: LedgerTransactionKind,
    entries: Vec<LedgerEntry>,
}

impl TransactionBuilder {
    pub fn new(kind: LedgerTransactionKind) -> Self {
        Self {
            kind,
            entries: Vec::new(),
        }
    }

    pub fn credit(
        mut self,
        account_id: crate::id::LedgerAccountId,
        amount: crate::money::Money,
    ) -> Result<Self, LedgerError> {
        self.entries.push(LedgerEntry::credit(account_id, amount)?);
        Ok(self)
    }

    pub fn debit(
        mut self,
        account_id: crate::id::LedgerAccountId,
        amount: crate::money::Money,
    ) -> Result<Self, LedgerError> {
        self.entries.push(LedgerEntry::debit(account_id, amount)?);
        Ok(self)
    }

    pub fn entry(mut self, entry: LedgerEntry) -> Self {
        self.entries.push(entry);
        self
    }

    pub fn build(self) -> Result<BalancedTransaction, LedgerError> {
        BalancedTransaction::new(self.kind, self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::LedgerAccountId;
    use crate::money::Money;
    use proptest::prelude::*;

    fn acct() -> LedgerAccountId {
        LedgerAccountId::new()
    }

    #[test]
    fn a_balanced_pair_is_accepted() {
        let amount = Money::new(100_000, Currency::Inr);
        let txn = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
            .debit(acct(), amount)
            .unwrap()
            .credit(acct(), amount)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(txn.entries().len(), 2);
        assert_eq!(txn.kind(), LedgerTransactionKind::PaymentCaptured);
        assert_eq!(txn.total_credited(Currency::Inr), 100_000);
    }

    #[test]
    fn an_unbalanced_transaction_is_rejected() {
        let result = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
            .debit(acct(), Money::new(100_000, Currency::Inr))
            .unwrap()
            .credit(acct(), Money::new(99_999, Currency::Inr))
            .unwrap()
            .build();

        match result {
            Err(LedgerError::Unbalanced(diffs)) => {
                assert_eq!(diffs, vec![(Currency::Inr, -1)]);
            }
            other => panic!("expected Unbalanced, got {other:?}"),
        }
    }

    #[test]
    fn single_entry_is_rejected() {
        let result = TransactionBuilder::new(LedgerTransactionKind::FeeCharged)
            .credit(acct(), Money::new(100, Currency::Inr))
            .unwrap()
            .build();
        assert!(matches!(result, Err(LedgerError::TooFewEntries(1))));
    }

    #[test]
    fn empty_transaction_is_rejected() {
        let result = BalancedTransaction::new(LedgerTransactionKind::FeeCharged, vec![]);
        assert!(matches!(result, Err(LedgerError::TooFewEntries(0))));
    }

    #[test]
    fn a_three_way_split_balances() {
        // Fee of 2360 paise: merchant debited, revenue and tax credited.
        let txn = TransactionBuilder::new(LedgerTransactionKind::FeeCharged)
            .debit(acct(), Money::new(2360, Currency::Inr))
            .unwrap()
            .credit(acct(), Money::new(2000, Currency::Inr))
            .unwrap()
            .credit(acct(), Money::new(360, Currency::Inr))
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(txn.entries().len(), 3);
        assert_eq!(txn.total_credited(Currency::Inr), 2360);
    }

    #[test]
    fn each_currency_must_balance_independently() {
        // Sums to zero overall if you ignore currency, but each currency
        // is individually unbalanced. Must be rejected.
        let entries = vec![
            LedgerEntry::debit(acct(), Money::new(1000, Currency::Inr)).unwrap(),
            LedgerEntry::credit(acct(), Money::new(1000, Currency::Usd)).unwrap(),
        ];
        let result = BalancedTransaction::new(LedgerTransactionKind::PaymentCaptured, entries);
        assert!(matches!(result, Err(LedgerError::Unbalanced(_))));
    }

    #[test]
    fn multi_currency_transaction_is_valid_when_each_side_balances() {
        let entries = vec![
            LedgerEntry::debit(acct(), Money::new(1000, Currency::Inr)).unwrap(),
            LedgerEntry::credit(acct(), Money::new(1000, Currency::Inr)).unwrap(),
            LedgerEntry::debit(acct(), Money::new(500, Currency::Usd)).unwrap(),
            LedgerEntry::credit(acct(), Money::new(500, Currency::Usd)).unwrap(),
        ];
        let txn =
            BalancedTransaction::new(LedgerTransactionKind::PaymentCaptured, entries).unwrap();
        assert_eq!(txn.currencies().len(), 2);
    }

    proptest! {
        /// For any set of amounts, a transaction built by debiting each amount
        /// and crediting the total must always balance. This is the core
        /// accounting invariant, checked against hundreds of random inputs.
        #[test]
        fn any_split_that_sums_to_the_total_balances(
            amounts in prop::collection::vec(1i64..1_000_000, 1..12)
        ) {
            let total: i64 = amounts.iter().sum();

            let mut builder = TransactionBuilder::new(LedgerTransactionKind::FeeCharged)
                .debit(acct(), Money::new(total, Currency::Inr))
                .unwrap();

            for a in &amounts {
                builder = builder.credit(acct(), Money::new(*a, Currency::Inr)).unwrap();
            }

            prop_assert!(builder.build().is_ok());
        }

        /// Conversely, if the credit side is off by any non-zero delta, the
        /// transaction must be rejected. No delta may slip through.
        #[test]
        fn any_nonzero_imbalance_is_rejected(
            base in 1i64..1_000_000,
            delta in prop::sample::select(vec![-1000i64, -7, -1, 1, 7, 1000])
        ) {
            let result = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
                .debit(acct(), Money::new(base, Currency::Inr))
                .unwrap()
                .credit(acct(), Money::new(base + delta, Currency::Inr))
                .unwrap()
                .build();

            prop_assert!(matches!(result, Err(LedgerError::Unbalanced(_))));
        }
    }
}
