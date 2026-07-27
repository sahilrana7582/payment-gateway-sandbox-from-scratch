use serde::{Deserialize, Serialize};

use crate::{
    id::{LedgerAccountId, MerchantId},
    ledger::{account_type::AccountType, ledger_error::LedgerError},
    money::Currency,
};

/// A ledger account. One row per (merchant, type, currency) for merchant-scoped
/// accounts, one row per (type, currency) for platform accounts.
///
/// Note there is no `balance` field. Balance is always derived as
/// `SUM(ledger_entries.amount)` for this account. A mutable balance column is
/// the single most common way real ledgers go wrong — it can drift from the
/// entries, and once it does you cannot tell which one is lying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerAccount {
    pub id: LedgerAccountId,
    pub merchant_id: Option<MerchantId>,
    pub account_type: AccountType,
    pub currency: Currency,
}

impl LedgerAccount {
    /// Construct a merchant-scoped account. Rejects platform-only account types.
    pub fn for_merchant(
        merchant_id: MerchantId,
        account_type: AccountType,
        currency: Currency,
    ) -> Result<Self, LedgerError> {
        if !account_type.is_merchant_scoped() {
            return Err(LedgerError::InvalidAccountScope { account_type });
        }

        Ok(Self {
            id: LedgerAccountId::new(),
            merchant_id: Some(merchant_id),
            account_type,
            currency,
        })
    }

    /// Construct a platform-scoped account. Rejects merchant account types.
    pub fn for_platform(
        account_type: AccountType,
        currency: Currency,
    ) -> Result<Self, LedgerError> {
        if account_type.is_merchant_scoped() {
            return Err(LedgerError::InvalidAccountScope { account_type });
        }
        Ok(Self {
            id: LedgerAccountId::new(),
            merchant_id: None,
            account_type,
            currency,
        })
    }

    pub fn from_parts(
        id: LedgerAccountId,
        merchant_id: Option<MerchantId>,
        account_type: AccountType,
        currency: Currency,
    ) -> Result<Self, LedgerError> {
        let scoped = account_type.is_merchant_scoped();

        if scoped != merchant_id.is_some() {
            return Err(LedgerError::InvalidAccountScope { account_type });
        }

        Ok(Self {
            id,
            merchant_id,
            account_type,
            currency,
        })
    }
}

#[cfg(test)]
pub mod tests {
    use crate::ledger::account_type::BalanceDirection;

    use super::*;

    #[test]
    fn merchant_account_can_not_be_platform_account() {
        let merchant_id = MerchantId::new();

        assert!(LedgerAccount::for_merchant(
            merchant_id,
            AccountType::PlatformRevenue,
            Currency::Inr
        )
        .is_err());
    }

    #[test]
    fn merchant_accounts_require_a_merchant_id() {
        let merchant = MerchantId::new();
        assert!(
            LedgerAccount::for_merchant(merchant, AccountType::MerchantPending, Currency::Inr)
                .is_ok()
        );
        // A platform account type cannot be merchant-scoped.
        assert!(
            LedgerAccount::for_merchant(merchant, AccountType::PlatformRevenue, Currency::Inr)
                .is_err()
        );
    }

    #[test]
    fn platform_accounts_reject_merchant_types() {
        assert!(LedgerAccount::for_platform(AccountType::PlatformRevenue, Currency::Inr).is_ok());
        assert!(LedgerAccount::for_platform(AccountType::TaxPayable, Currency::Inr).is_ok());
        assert!(LedgerAccount::for_platform(AccountType::MerchantPending, Currency::Inr).is_err());
    }

    #[test]
    fn from_parts_rejects_mismatched_scoping() {
        // Platform type with a merchant id -> invalid.
        let bad = LedgerAccount::from_parts(
            LedgerAccountId::new(),
            Some(MerchantId::new()),
            AccountType::PlatformRevenue,
            Currency::Inr,
        );
        assert!(bad.is_err());

        // Merchant type without a merchant id -> invalid.
        let bad2 = LedgerAccount::from_parts(
            LedgerAccountId::new(),
            None,
            AccountType::MerchantPending,
            Currency::Inr,
        );
        assert!(bad2.is_err());
    }

    #[test]
    fn scoping_and_normal_balance_defined_for_every_variant() {
        // This loop is the guard: if someone adds a variant and forgets to
        // classify it, the `match` in normal_balance() won't compile at all.
        for &t in AccountType::all() {
            let _ = t.normal_balance();
            let _ = t.is_merchant_scoped();
            assert!(!t.as_str().is_empty());
        }
    }

    #[test]
    fn account_type_roundtrips_through_string() {
        for &t in AccountType::all() {
            let parsed: AccountType = t.as_str().parse().unwrap();
            assert_eq!(parsed, t);
        }
    }

    #[test]
    fn liability_accounts_are_credit_normal() {
        assert_eq!(
            AccountType::MerchantPending.normal_balance(),
            BalanceDirection::Credit
        );
        assert_eq!(
            AccountType::TaxPayable.normal_balance(),
            BalanceDirection::Credit
        );
        assert_eq!(
            AccountType::GatewayClearing.normal_balance(),
            BalanceDirection::Debit
        );
    }
}
