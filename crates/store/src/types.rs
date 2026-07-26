
//! The bridge between domain types and their SQL representations.
//!
//! Design rule: the `domain` crate stays free of database concerns, so the
//! conversions live here, not there. Three kinds of mapping:
//!
//! 1. **Enums** — handled by `#[cfg_attr(feature = "db", derive(sqlx::Type))]`
//!    on the domain enums themselves (see each domain file). Those map 1:1 to
//!    Postgres `CREATE TYPE` enums and need nothing here.
//!
//! 2. **`Id<T>`** — every typed ID is a UUID in the database. sqlx can't derive
//!    an impl for our generic `Id<T>`, so we provide helpers to go
//!    `Id<T> <-> Uuid` at the query boundary. Repositories bind `id.as_uuid()`
//!    and reconstruct with `Id::from_uuid(row_uuid)`.
//!
//! 3. **`Money`** — stored as TWO columns (`amount_minor BIGINT`, `currency
//!    CHAR(3)`), never one. This module has the helpers to split a `Money` into
//!    its columns on write and rebuild it on read.

use domain::money::{Currency, Money};

/// Split a `Money` into the pair of values bound to `(amount_minor, currency)`
/// columns. Currency is emitted as its ISO code string.
///
/// ```ignore
/// let (minor, currency) = money_to_columns(order.amount);
/// sqlx::query!("INSERT ... (amount_minor, currency) VALUES ($1, $2)", minor, currency)
/// ```
pub fn money_to_columns(money: Money) -> (i64, String) {
    (money.minor(), money.currency().code().to_string())
}

/// Rebuild a `Money` from the two columns read out of a row. Returns a decode
/// error if the stored currency string isn't one we recognize (schema drift or
/// manual tampering).
pub fn money_from_columns(minor: i64, currency: &str) -> Result<Money, super::error::StoreError> {
    let currency: Currency = currency
        .parse()
        .map_err(|_| super::error::StoreError::decode("currency", currency))?;
    Ok(Money::new(minor, currency))
}

/// Parse just a currency column on its own (used by tables that store a bare
/// currency without an amount, e.g. ledger_accounts).
pub fn currency_from_column(currency: &str) -> Result<Currency, super::error::StoreError> {
    currency
        .parse()
        .map_err(|_| super::error::StoreError::decode("currency", currency))
}

/// Convert a domain `Scope` slice to the `TEXT[]` representation stored on
/// `api_keys.scopes`, and back. Scopes are stored as their string form
/// (`"payments:write"`), so a human reading the row can see the grants.
pub mod scopes {
    use domain::api_key::Scope;

    pub fn to_strings(scopes: &[Scope]) -> Vec<String> {
        scopes.iter().map(|s| s.as_str().to_string()).collect()
    }

    pub fn from_strings(raw: &[String]) -> Vec<Scope> {
        raw.iter().filter_map(|s| parse_scope(s)).collect()
    }

    fn parse_scope(s: &str) -> Option<Scope> {
        Some(match s {
            "orders:read"     => Scope::OrdersRead,
            "orders:write"    => Scope::OrdersWrite,
            "payments:read"   => Scope::PaymentsRead,
            "payments:write"  => Scope::PaymentsWrite,
            "refunds:write"   => Scope::RefundsWrite,
            "webhooks:manage" => Scope::WebhooksManage,
            _ => return None, // unknown scope string is ignored, not fatal
        })
    }
}

/// Helpers for `Id<T> <-> Uuid`. These are thin, but centralizing them means a
/// repository never reaches into `Id`'s internals directly.
pub mod ids {
    use domain::id::{Id, Prefixed};
    use uuid::Uuid;

    pub fn to_uuid<T>(id: &Id<T>) -> Uuid {
        *id.as_uuid()
    }

    pub fn from_uuid<T: Prefixed>(uuid: Uuid) -> Id<T> {
        Id::from_uuid(uuid)
    }

    /// For nullable FK columns: `Option<Id<T>> <-> Option<Uuid>`.
    pub fn opt_to_uuid<T>(id: Option<&Id<T>>) -> Option<Uuid> {
        id.map(|i| *i.as_uuid())
    }

    pub fn opt_from_uuid<T: Prefixed>(uuid: Option<Uuid>) -> Option<Id<T>> {
        uuid.map(Id::from_uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_roundtrips_through_columns() {
        let original = Money::new(150_000, Currency::Inr);
        let (minor, currency) = money_to_columns(original);
        assert_eq!(minor, 150_000);
        assert_eq!(currency, "INR");

        let rebuilt = money_from_columns(minor, &currency).unwrap();
        assert_eq!(rebuilt, original);
    }

    #[test]
    fn money_from_columns_rejects_unknown_currency() {
        let result = money_from_columns(1000, "XXX");
        assert!(result.is_err());
    }

    #[test]
    fn negative_money_survives_roundtrip() {
        // Ledger debits are negative — must survive the column split intact.
        let original = Money::new(-750, Currency::Usd);
        let (minor, currency) = money_to_columns(original);
        let rebuilt = money_from_columns(minor, &currency).unwrap();
        assert_eq!(rebuilt, original);
        assert!(rebuilt.is_negative());
    }

    #[test]
    fn scope_strings_roundtrip() {
        use domain::api_key::Scope;
        let original = Scope::full_access().to_vec();
        let strings = scopes::to_strings(&original);
        assert!(strings.contains(&"payments:write".to_string()));
        let rebuilt = scopes::from_strings(&strings);
        assert_eq!(rebuilt, original);
    }

    #[test]
    fn unknown_scope_string_is_dropped_not_fatal() {
        let raw = vec!["payments:read".to_string(), "bogus:scope".to_string()];
        let parsed = scopes::from_strings(&raw);
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn id_uuid_roundtrip() {
        use domain::id::OrderId;
        let original = OrderId::new();
        let uuid = ids::to_uuid(&original);
        let rebuilt: OrderId = ids::from_uuid(uuid);
        assert_eq!(original, rebuilt);
    }
}