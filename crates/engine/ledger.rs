//! Posting rules — the accounting policy of the gateway. Given a business
//! event (capture, refund), which accounts move and by how much.
//!
//! Every rule builds a `BalancedTransaction`, so an unbalanced posting is
//! unrepresentable, and the DB trigger re-verifies at COMMIT. All functions
//! take `&mut PgTx` — postings always ride the caller's transaction.

use domain::id::MerchantId;
use domain::ledger::account_type::AccountType;
use domain::ledger::ledger_transaction::{LedgerTransactionKind, TransactionBuilder};
use domain::money::Money;
use store::tx::PgTx;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::EngineResult;
use crate::fees::FeeBreakdown;

/// Postings for a captured payment. Two ledger transactions:
///
///   payment_captured:  debit  gateway_clearing   amount
///                      credit merchant_pending   amount
///
///   fee_charged:       debit  merchant_pending   fee.total
///                      credit platform_revenue   fee.base
///                      credit tax_payable        fee.tax    (skipped if 0)
///
/// The tax entry is conditional because `LedgerEntry` rejects zero amounts —
/// a zero entry carries no information and is always a bug.
pub async fn post_capture(
    tx: &mut PgTx<'_>,
    merchant_id: MerchantId,
    amount: Money,
    fees: &FeeBreakdown,
    payment_uuid: Uuid,
    now: OffsetDateTime,
) -> EngineResult<()> {
    let currency = amount.currency();

    let clearing =
        store::ledger::find_platform_account(&mut **tx, AccountType::GatewayClearing, currency)
            .await?;
    let pending = store::ledger::find_or_create_merchant_account(
        tx,
        merchant_id,
        AccountType::MerchantPending,
        currency,
        now,
    )
    .await?;

    let capture = TransactionBuilder::new(LedgerTransactionKind::PaymentCaptured)
        .debit(clearing.id, amount)?
        .credit(pending.id, amount)?
        .build()?;
    store::ledger::post(tx, &capture, Some("payment"), Some(payment_uuid), None, now).await?;

    if fees.total > 0 {
        let revenue =
            store::ledger::find_platform_account(&mut **tx, AccountType::PlatformRevenue, currency)
                .await?;

        let mut builder = TransactionBuilder::new(LedgerTransactionKind::FeeCharged)
            .debit(pending.id, Money::new(fees.total, currency))?
            .credit(revenue.id, Money::new(fees.base, currency))?;

        if fees.tax > 0 {
            let tax_acct =
                store::ledger::find_platform_account(&mut **tx, AccountType::TaxPayable, currency)
                    .await?;
            builder = builder.credit(tax_acct.id, Money::new(fees.tax, currency))?;
        }

        let fee_txn = builder.build()?;
        store::ledger::post(tx, &fee_txn, Some("payment"), Some(payment_uuid), None, now).await?;
    }

    Ok(())
}

/// The portion of a payment's fee being returned to the merchant on a refund.
///
/// Split so the ledger can reverse `platform_revenue` and `tax_payable`
/// independently — the tax authority's share must come back out of the tax
/// account, not be netted against revenue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRefund {
    pub base: i64,
    pub tax: i64,
}

impl FeeRefund {
    #[must_use]
    pub const fn total(&self) -> i64 {
        self.base + self.tax
    }

    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.base == 0 && self.tax == 0
    }
}

/// Postings for a refund. Up to two ledger transactions.
///
/// ```text
/// refund_issued:  debit  merchant_pending   refund_amount
///                 credit gateway_clearing   refund_amount
///
/// fee_refunded:   debit  platform_revenue   fee.base   (skipped if 0)
///                 debit  tax_payable        fee.tax    (skipped if 0)
///                 credit merchant_pending   fee.total
/// ```
///
/// Zero-amount entries are skipped rather than posted: `LedgerEntry` rejects
/// them, and a zero entry carries no information. That is why the fee
/// transaction is assembled conditionally instead of unconditionally — a USD
/// payment has no tax component, and a very small partial refund may round to
/// no fee return at all.
pub async fn post_refund(
    tx: &mut PgTx<'_>,
    merchant_id: MerchantId,
    refund_amount: Money,
    fee: FeeRefund,
    refund_uuid: Uuid,
    now: OffsetDateTime,
) -> EngineResult<()> {
    let currency = refund_amount.currency();

    let clearing =
        store::ledger::find_platform_account(&mut **tx, AccountType::GatewayClearing, currency)
            .await?;
    let pending = store::ledger::find_or_create_merchant_account(
        tx,
        merchant_id,
        AccountType::MerchantPending,
        currency,
        now,
    )
    .await?;

    let refund_txn = TransactionBuilder::new(LedgerTransactionKind::RefundIssued)
        .debit(pending.id, refund_amount)?
        .credit(clearing.id, refund_amount)?
        .build()?;
    store::ledger::post(
        tx,
        &refund_txn,
        Some("refund"),
        Some(refund_uuid),
        None,
        now,
    )
    .await?;

    if fee.is_zero() {
        return Ok(());
    }

    let mut builder = TransactionBuilder::new(LedgerTransactionKind::FeeRefunded)
        .credit(pending.id, Money::new(fee.total(), currency))?;

    if fee.base > 0 {
        let revenue =
            store::ledger::find_platform_account(&mut **tx, AccountType::PlatformRevenue, currency)
                .await?;
        builder = builder.debit(revenue.id, Money::new(fee.base, currency))?;
    }
    if fee.tax > 0 {
        let tax_acct =
            store::ledger::find_platform_account(&mut **tx, AccountType::TaxPayable, currency)
                .await?;
        builder = builder.debit(tax_acct.id, Money::new(fee.tax, currency))?;
    }

    let fee_txn = builder.build()?;
    store::ledger::post(tx, &fee_txn, Some("refund"), Some(refund_uuid), None, now).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fees::FeeSchedule;
    use domain::money::Currency;
    use sqlx::PgPool;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    async fn seed_merchant(pool: &PgPool) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        store::merchant::insert(pool, &m).await.unwrap();
        m.id
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn capture_posting_balances_and_lands_in_the_right_accounts(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let amount = Money::new(150_000, Currency::Inr);
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(amount)
            .unwrap();

        let mut tx = store::tx::begin(&pool).await.unwrap();
        post_capture(&mut tx, mid, amount, &fees, Uuid::now_v7(), now())
            .await
            .unwrap();
        store::tx::commit(tx).await.unwrap();

        // Merchant pending = amount - total fee.
        let balances = store::ledger::balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        let pending = balances
            .iter()
            .find(|b| b.account_type == AccountType::MerchantPending)
            .unwrap();
        assert_eq!(pending.balance.minor(), 150_000 - 3_894);

        // Platform accounts hold the fee split.
        let revenue = store::ledger::find_platform_account(
            &pool,
            AccountType::PlatformRevenue,
            Currency::Inr,
        )
        .await
        .unwrap();
        assert_eq!(
            store::ledger::balance(&pool, revenue.id).await.unwrap(),
            3_300
        );

        let tax =
            store::ledger::find_platform_account(&pool, AccountType::TaxPayable, Currency::Inr)
                .await
                .unwrap();
        assert_eq!(store::ledger::balance(&pool, tax.id).await.unwrap(), 594);

        let clearing = store::ledger::find_platform_account(
            &pool,
            AccountType::GatewayClearing,
            Currency::Inr,
        )
        .await
        .unwrap();
        assert_eq!(
            store::ledger::balance(&pool, clearing.id).await.unwrap(),
            -150_000
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn usd_capture_skips_the_zero_tax_entry(pool: PgPool) {
        // Would fail with ZeroAmountEntry if the tax entry weren't conditional.
        let mid = seed_merchant(&pool).await;
        let amount = Money::new(150_000, Currency::Usd);
        let fees = FeeSchedule::for_currency(Currency::Usd)
            .compute(amount)
            .unwrap();
        assert_eq!(fees.tax, 0);

        let mut tx = store::tx::begin(&pool).await.unwrap();
        post_capture(&mut tx, mid, amount, &fees, Uuid::now_v7(), now())
            .await
            .unwrap();
        store::tx::commit(tx).await.unwrap();
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn refund_reverses_the_capture(pool: PgPool) {
        let mid = seed_merchant(&pool).await;
        let amount = Money::new(150_000, Currency::Inr);
        let fees = FeeSchedule::for_currency(Currency::Inr)
            .compute(amount)
            .unwrap();

        let mut tx = store::tx::begin(&pool).await.unwrap();
        post_capture(&mut tx, mid, amount, &fees, Uuid::now_v7(), now())
            .await
            .unwrap();

        let refund_fee = FeeRefund { base: 2, tax: 1 };
        post_refund(
            &mut tx,
            mid,
            Money::new(50_000, Currency::Inr),
            refund_fee,
            Uuid::now_v7(),
            now(),
        )
        .await
        .unwrap();
        store::tx::commit(tx).await.unwrap();

        let balances = store::ledger::balances_for_merchant(&pool, mid, Currency::Inr)
            .await
            .unwrap();
        let pending = balances
            .iter()
            .find(|b| b.account_type == AccountType::MerchantPending)
            .unwrap();
        // amount - fee - refund
        assert_eq!(
            pending.balance.minor(),
            150_000 - fees.total - 50_000 + refund_fee.base + refund_fee.tax,
        );
    }
}
