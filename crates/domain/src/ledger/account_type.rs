use serde::{Deserialize, Serialize};

/// Which direction increases an account's balance, in classical accounting terms.
///
/// - **Debit-normal** accounts (assets, expenses) increase when debited.
/// - **Credit-normal** accounts (liabilities, revenue, equity) increase when credited.
///
/// We store this because it drives display: a merchant's payable balance should
/// render as a positive number even though it is a liability to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BalanceDirection {
    Debit,
    Credit,
}

/// The chart of accounts. Every ledger entry targets one of these.
///
/// Sign convention used throughout this codebase: a **positive** entry amount
/// is a **credit**, a **negative** amount is a **debit**. A transaction is
/// valid only when all entries sum to exactly zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "db", derive(sqlx::Type))]
#[cfg_attr(
    feature = "db",
    sqlx(type_name = "ledger_account_type", rename_all = "snake_case")
)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    /// Funds in flight from the card network. Debited when a payment captures.
    /// Represents the gateway's claim on the acquiring bank.
    GatewayClearing,

    /// Money owed to the merchant that has not yet cleared the settlement
    /// window. Credited on capture, debited when it moves to available.
    MerchantPending,

    /// Money owed to the merchant that has cleared and can be paid out.
    MerchantAvailable,

    /// Money withheld from the merchant as a risk reserve.
    MerchantReserve,

    /// Funds frozen while a dispute is open. Moved back on win, out on loss.
    DisputeHolding,

    /// The platform's fee income. Platform-scoped, never merchant-scoped.
    PlatformRevenue,

    /// Tax collected on fees (e.g. GST on the processing fee), owed to the
    /// tax authority. Platform-scoped.
    TaxPayable,
}

impl AccountType {
    /// Which side increases this account.
    ///
    /// Exhaustive match with no wildcard: adding a new account type will not
    /// compile until you classify it. That compile error is the point.
    pub const fn normal_balance(self) -> BalanceDirection {
        use AccountType::*;
        match self {
            // Asset-like: we hold or are owed these funds.
            GatewayClearing | DisputeHolding => BalanceDirection::Debit,

            // Liability-like: we owe these funds to someone else.
            MerchantPending | MerchantAvailable | MerchantReserve | TaxPayable => {
                BalanceDirection::Credit
            }

            // Revenue: credit-normal.
            PlatformRevenue => BalanceDirection::Credit,
        }
    }

    /// Whether this account belongs to a specific merchant.
    ///
    /// Platform accounts (revenue, tax, clearing) are singletons per currency
    /// and must have `merchant_id == None`. Merchant accounts must have `Some`.
    /// Violating this silently would let one merchant's balance leak into
    /// another's, so we enforce it at construction.
    pub const fn is_merchant_scoped(self) -> bool {
        use AccountType::*;
        match self {
            MerchantPending | MerchantAvailable | MerchantReserve | DisputeHolding => true,
            GatewayClearing | PlatformRevenue | TaxPayable => false,
        }
    }

    pub const fn as_str(self) -> &'static str {
        use AccountType::*;
        match self {
            GatewayClearing => "gateway_clearing",
            MerchantPending => "merchant_pending",
            MerchantAvailable => "merchant_available",
            MerchantReserve => "merchant_reserve",
            DisputeHolding => "dispute_holding",
            PlatformRevenue => "platform_revenue",
            TaxPayable => "tax_payable",
        }
    }

    /// Every variant, for exhaustive tests and account provisioning.
    pub const fn all() -> &'static [AccountType] {
        use AccountType::*;
        &[
            GatewayClearing,
            MerchantPending,
            MerchantAvailable,
            MerchantReserve,
            DisputeHolding,
            PlatformRevenue,
            TaxPayable,
        ]
    }
}

impl std::fmt::Display for AccountType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AccountType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use AccountType::*;
        match s {
            "gateway_clearing" => Ok(GatewayClearing),
            "merchant_pending" => Ok(MerchantPending),
            "merchant_available" => Ok(MerchantAvailable),
            "merchant_reserve" => Ok(MerchantReserve),
            "dispute_holding" => Ok(DisputeHolding),
            "platform_revenue" => Ok(PlatformRevenue),
            "tax_payable" => Ok(TaxPayable),
            other => Err(format!("unknown account type: {other}")),
        }
    }
}
