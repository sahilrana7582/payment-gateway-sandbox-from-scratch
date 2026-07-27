//! Ledger persistence — the heart of the money system.
//!
//! Two responsibilities:
//!   1. `post` writes a `BalancedTransaction` (header + all entries) inside the
//!      caller's transaction. The domain type already guarantees the entries
//!      sum to zero; the DB trigger from migration 0013 re-verifies at COMMIT.
//!      Two independent layers — a bug in either is caught by the other.
//!   2. `balance` / `balances_for_merchant` compute balances as SUM(entries).
//!      There is NO stored balance column anywhere. Balance is always derived,
//!      so it can never drift from the entries that define it.
//!
//! `post` MUST take `&mut PgTx`, never `&PgPool`: a ledger posting is always
//! part of a larger unit (a capture, a refund) and must commit or roll back
//! atomically with it. There is deliberately no pool-based `post`.

use domain::id::{LedgerAccountId, LedgerTransactionId, MerchantId};
use domain::ledger::{
    account_type::AccountType,
    ledger_account::LedgerAccount,
    ledger_entry::LedgerEntry,
    ledger_transaction::{BalancedTransaction, LedgerTransactionKind},
};
use domain::money::{Currency, Money};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::tx::PgTx;
use crate::types::{currency_from_column, money_from_columns};

// ============================================================================
// Accounts
// ============================================================================

struct AccountRow {
    id: Uuid,
    merchant_id: Option<Uuid>,
    account_type: AccountType,
    currency: String,
}

impl AccountRow {
    fn into_domain(self) -> StoreResult<LedgerAccount> {
        let currency = currency_from_column(&self.currency)?;
        // from_parts re-validates the scoping invariant (platform accounts have
        // no merchant_id, merchant accounts do), so a corrupt row can't enter.
        LedgerAccount::from_parts(
            LedgerAccountId::from_uuid(self.id),
            self.merchant_id.map(MerchantId::from_uuid),
            self.account_type,
            currency,
        )
        .map_err(|e| StoreError::decode("ledger_account", e.to_string()))
    }
}

/// Insert a ledger account. Used when a merchant first transacts in a currency
/// (merchant-scoped accounts are created on demand; platform accounts are
/// seeded by migration 0021).
pub async fn insert_account(tx: &mut PgTx<'_>, account: &LedgerAccount) -> StoreResult<()> {
    sqlx::query!(
        r#"
        INSERT INTO ledger_accounts (id, merchant_id, account_type, currency, created_at)
        VALUES ($1, $2, $3, $4, now())
        "#,
        account.id.as_uuid(),
        account.merchant_id.map(|m| m.into_uuid()),
        account.account_type as AccountType,
        account.currency.code(),
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Find a platform account (merchant_id IS NULL) by type + currency. These are
/// singletons seeded at migration time, so this is a lookup, never a create.
pub async fn find_platform_account<'e, E>(
    executor: E,
    account_type: AccountType,
    currency: Currency,
) -> StoreResult<LedgerAccount>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        AccountRow,
        r#"
        SELECT id, merchant_id, account_type AS "account_type: AccountType", currency
        FROM ledger_accounts
        WHERE account_type = $1 AND currency = $2 AND merchant_id IS NULL
        "#,
        account_type as AccountType,
        currency.code(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("platform ledger account"))?;

    row.into_domain()
}

/// Find a merchant account by (merchant, type, currency).
pub async fn find_merchant_account<'e, E>(
    executor: E,
    merchant_id: MerchantId,
    account_type: AccountType,
    currency: Currency,
) -> StoreResult<LedgerAccount>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as!(
        AccountRow,
        r#"
        SELECT id, merchant_id, account_type AS "account_type: AccountType", currency
        FROM ledger_accounts
        WHERE merchant_id = $1 AND account_type = $2 AND currency = $3
        "#,
        merchant_id.as_uuid(),
        account_type as AccountType,
        currency.code(),
    )
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| StoreError::not_found("merchant ledger account"))?;

    row.into_domain()
}

/// Find a merchant account, creating it if absent. This is the on-demand
/// provisioning path the engine uses before its first posting for a merchant
/// in a given currency. Runs in the caller's transaction so provisioning and
/// posting commit together. The unique index makes a concurrent double-create
/// resolve as: one wins, the other gets a conflict, and we re-read.
pub async fn find_or_create_merchant_account(
    tx: &mut PgTx<'_>,
    merchant_id: MerchantId,
    account_type: AccountType,
    currency: Currency,
    now: OffsetDateTime,
) -> StoreResult<LedgerAccount> {
    // Fast path: already exists.
    if let Ok(acct) = find_merchant_account(&mut **tx, merchant_id, account_type, currency).await {
        return Ok(acct);
    }

    // Create. If a concurrent tx beat us, the unique index yields a conflict;
    // we swallow it and re-read so both callers end with the same account.
    let account = LedgerAccount::for_merchant(merchant_id, account_type, currency)
        .map_err(|e| StoreError::decode("ledger_account", e.to_string()))?;

    let insert_result = sqlx::query!(
        r#"
        INSERT INTO ledger_accounts (id, merchant_id, account_type, currency, created_at)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT DO NOTHING
        "#,
        account.id.as_uuid(),
        Some(merchant_id.as_uuid()),
        account_type as AccountType,
        currency.code(),
        now,
    )
    .execute(&mut **tx)
    .await?;

    if insert_result.rows_affected() == 1 {
        Ok(account)
    } else {
        // Someone else created it; read theirs.
        find_merchant_account(&mut **tx, merchant_id, account_type, currency).await
    }
}

// ============================================================================
// Posting a balanced transaction
// ============================================================================

/// Write a balanced transaction: the header row plus every entry, all inside
/// the caller's transaction. Returns the transaction id.
///
/// The `BalancedTransaction` type guarantees the entries sum to zero before we
/// get here. The DB constraint trigger (migration 0013, DEFERRABLE INITIALLY
/// DEFERRED) re-checks at COMMIT — so if this posting is ever combined with a
/// buggy raw insert that unbalances it, the whole COMMIT fails. If the trigger
/// fires it surfaces as `StoreError::LedgerIntegrity`.
pub async fn post(
    tx: &mut PgTx<'_>,
    txn: &BalancedTransaction,
    reference_type: Option<&str>,
    reference_id: Option<Uuid>,
    description: Option<&str>,
    now: OffsetDateTime,
) -> StoreResult<LedgerTransactionId> {
    let txn_id = LedgerTransactionId::new();

    sqlx::query!(
        r#"
        INSERT INTO ledger_transactions
            (id, kind, reference_type, reference_id, description, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        txn_id.as_uuid(),
        txn.kind() as LedgerTransactionKind,
        reference_type,
        reference_id,
        description,
        now,
    )
    .execute(&mut **tx)
    .await?;

    for entry in txn.entries() {
        sqlx::query!(
            r#"
            INSERT INTO ledger_entries
                (id, transaction_id, account_id, amount_minor, currency, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            Uuid::from(uuid::Uuid::now_v7()),
            txn_id.as_uuid(),
            entry.account_id.as_uuid(),
            entry.amount.minor(),
            entry.amount.currency().code(),
            now,
        )
        .execute(&mut **tx)
        .await?;
    }

    Ok(txn_id)
}

// ============================================================================
// Balances — always SUM(entries), never a stored column
// ============================================================================

/// Balance of one account: SUM(amount_minor) over its entries. Zero if the
/// account has no entries yet.
pub async fn balance<'e, E>(executor: E, account_id: LedgerAccountId) -> StoreResult<i64>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query!(
        r#"
        SELECT COALESCE(SUM(amount_minor)::bigint, 0) AS "balance!"
        FROM ledger_entries
        WHERE account_id = $1
        "#,
        account_id.as_uuid(),
    )
    .fetch_one(executor)
    .await?;

    Ok(row.balance)
}

/// A single row of a merchant's balance breakdown.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountBalance {
    pub account_type: AccountType,
    pub balance: Money,
}

/// All of a merchant's account balances in a currency, computed from entries.
/// The dashboard's balance view and the settlement job both use this.
pub async fn balances_for_merchant(
    pool: &PgPool,
    merchant_id: MerchantId,
    currency: Currency,
) -> StoreResult<Vec<AccountBalance>> {
    let rows = sqlx::query!(
        r#"
        SELECT a.account_type AS "account_type: AccountType",
               COALESCE(SUM(e.amount_minor), 0)::bigint AS "balance!"
        FROM ledger_accounts a
        LEFT JOIN ledger_entries e ON e.account_id = a.id
        WHERE a.merchant_id = $1 AND a.currency = $2
        GROUP BY a.account_type
        "#,
        merchant_id.as_uuid(),
        currency.code(),
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| AccountBalance {
            account_type: r.account_type,
            balance: Money::new(r.balance, currency),
        })
        .collect())
}

/// Fetch every entry for a ledger transaction (the dashboard's T-account view).
pub async fn entries_for_transaction(
    pool: &PgPool,
    transaction_id: LedgerTransactionId,
) -> StoreResult<Vec<LedgerEntry>> {
    let rows = sqlx::query!(
        r#"
        SELECT account_id, amount_minor, currency
        FROM ledger_entries
        WHERE transaction_id = $1
        ORDER BY amount_minor DESC
        "#,
        transaction_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            let money = money_from_columns(r.amount_minor, &r.currency)?;
            LedgerEntry::from_signed(LedgerAccountId::from_uuid(r.account_id), money)
                .map_err(|e| StoreError::decode("ledger_entry", e.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ledger::ledger_transaction::TransactionBuilder;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        crate::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    #[sqlx::test(migrations = "../../migrations")]

    async fn platform_accounts_are_seeded(pool: PgPool) {
        // Migration 0021 seeds these. Just look one up.
        let clearing = find_platform_account(&pool, AccountType::GatewayClearing, Currency::Inr)
            .await
            .unwrap();
        assert_eq!(clearing.account_type, AccountType::GatewayClearing);
        assert!(clearing.merchant_id.is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]

    async fn post_a_balanced_capture_and_read_balances(pool: PgPool) {
        let mid = seed_merchant(&pool).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();

        let clearing = find_platform_account(&mut *tx, AccountType::GatewayClearing, Currency::Inr)
            .await
            .unwrap();
        let pending = find_or_create_merchant_account(
            &mut tx,
            mid,
            AccountType::MerchantPending,
            Currency::Inr,
            now(),
        )
        .await
        .unwrap();

        // Capture ₹1500: debit clearing, credit merchant pending.
        let amount = Money::new(150_000, Currency::Inr);
        let txn = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
            .debit(clearing.id, amount)
            .unwrap()
            .credit(pending.id, amount)
            .unwrap()
            .build()
            .unwrap();

        post(&mut tx, &txn, Some("payment"), None, None, now())
            .await
            .unwrap();
        crate::tx::commit(tx).await.unwrap();

        // Merchant pending should now be +150000.
        let balances = balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        let pending_bal = balances
            .iter()
            .find(|b| b.account_type == AccountType::MerchantPending)
            .unwrap();
        assert_eq!(pending_bal.balance, Money::new(150_000, Currency::Inr));

        // Gateway clearing (platform) should be -150000.
        let clearing_bal = balance(&pool, clearing.id).await.unwrap();
        assert_eq!(clearing_bal, -150_000);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn find_or_create_is_idempotent(pool: PgPool) {
        let mid = seed_merchant(&pool).await;

        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let a = find_or_create_merchant_account(
            &mut tx,
            mid,
            AccountType::MerchantPending,
            Currency::Inr,
            now(),
        )
        .await
        .unwrap();
        let b = find_or_create_merchant_account(
            &mut tx,
            mid,
            AccountType::MerchantPending,
            Currency::Inr,
            now(),
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(a.id, b.id); // second call returned the same account
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn balance_of_empty_account_is_zero(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let mut tx = crate::tx::begin(&pool).await.unwrap();
        let acct = find_or_create_merchant_account(
            &mut tx,
            mid,
            AccountType::MerchantAvailable,
            Currency::Inr,
            now(),
        )
        .await
        .unwrap();
        crate::tx::commit(tx).await.unwrap();

        assert_eq!(balance(&pool, acct.id).await.unwrap(), 0);
    }
}
