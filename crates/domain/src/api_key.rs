use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::id::{ApiKeyId, MerchantId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    /// `sk_test_...` — server-to-server only. Never appears in browser code.
    Secret,
    /// `pk_test_...` — safe to expose in the browser. Can only open checkout
    /// and create tokens; cannot read or mutate payments/orders/refunds.
    Publishable,
}

impl ApiKeyKind {
    pub const fn prefix(self) -> &'static str {
        match self {
            ApiKeyKind::Secret => "sk_test",
            ApiKeyKind::Publishable => "pk_test",
        }
    }
}

/// Fine-grained permissions. A key's request is authorized only if it holds
/// the scope the handler requires — enforced in `api`'s auth middleware, not
/// left to convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    OrdersRead,
    OrdersWrite,
    PaymentsRead,
    PaymentsWrite,
    RefundsWrite,
    WebhooksManage,
}

impl Scope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::OrdersRead => "orders:read",
            Scope::OrdersWrite => "orders:write",
            Scope::PaymentsRead => "payments:read",
            Scope::PaymentsWrite => "payments:write",
            Scope::RefundsWrite => "refunds:write",
            Scope::WebhooksManage => "webhooks:manage",
        }
    }

    /// A freshly issued secret key gets every scope by default. Narrower
    /// keys are an explicit opt-in the merchant makes later, not the default.
    pub const fn full_access() -> &'static [Scope] {
        &[
            Scope::OrdersRead,
            Scope::OrdersWrite,
            Scope::PaymentsRead,
            Scope::PaymentsWrite,
            Scope::RefundsWrite,
            Scope::WebhooksManage,
        ]
    }

    /// A publishable key gets nothing from this list — it authenticates
    /// checkout sessions through a completely separate, more restricted path,
    /// never through this scope system.
    pub const fn publishable_default() -> &'static [Scope] {
        &[]
    }
}

/// An issued API key. The raw secret is never stored — only `secret_hash`
/// (Argon2id, produced by the `crypto` crate) lives here. The plaintext
/// secret is shown to the merchant exactly once, at creation time, by the
/// layer that calls `crypto::api_key::generate()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub merchant_id: MerchantId,
    pub kind: ApiKeyKind,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub secret_hash: String,
    pub last_used_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

impl ApiKey {
    pub fn new(
        merchant_id: MerchantId,
        kind: ApiKeyKind,
        label: impl Into<String>,
        secret_hash: impl Into<String>,
        now: OffsetDateTime,
    ) -> Self {
        let scopes = match kind {
            ApiKeyKind::Secret => Scope::full_access().to_vec(),
            ApiKeyKind::Publishable => Scope::publishable_default().to_vec(),
        };
        Self {
            id: ApiKeyId::new(),
            merchant_id,
            kind,
            label: label.into(),
            scopes,
            secret_hash: secret_hash.into(),
            last_used_at: None,
            revoked_at: None,
            created_at: now,
        }
    }

    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn is_active(&self) -> bool {
        !self.is_revoked()
    }

    pub fn revoke(&mut self, now: OffsetDateTime) {
        if self.revoked_at.is_none() {
            self.revoked_at = Some(now);
        }
    }

    pub fn record_use(&mut self, now: OffsetDateTime) {
        self.last_used_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    #[test]
    fn secret_key_gets_full_scope_by_default() {
        let key = ApiKey::new(
            MerchantId::new(),
            ApiKeyKind::Secret,
            "default",
            "hash",
            now(),
        );
        assert!(key.has_scope(Scope::PaymentsWrite));
        assert!(key.has_scope(Scope::RefundsWrite));
        assert!(key.is_active());
    }

    #[test]
    fn publishable_key_gets_no_server_scopes() {
        let key = ApiKey::new(
            MerchantId::new(),
            ApiKeyKind::Publishable,
            "web",
            "hash",
            now(),
        );
        assert!(!key.has_scope(Scope::PaymentsWrite));
        assert!(key.scopes.is_empty());
    }

    #[test]
    fn revoke_is_idempotent_on_the_timestamp() {
        let mut key = ApiKey::new(MerchantId::new(), ApiKeyKind::Secret, "k", "hash", now());
        key.revoke(now());
        let first = key.revoked_at;
        key.revoke(datetime!(2026-06-01 00:00:00 UTC));
        assert_eq!(key.revoked_at, first); // second revoke does not move the timestamp
    }

    #[test]
    fn revoked_key_is_not_active() {
        let mut key = ApiKey::new(MerchantId::new(), ApiKeyKind::Secret, "k", "hash", now());
        assert!(key.is_active());
        key.revoke(now());
        assert!(!key.is_active());
    }

    #[test]
    fn prefixes_are_distinct() {
        assert_ne!(
            ApiKeyKind::Secret.prefix(),
            ApiKeyKind::Publishable.prefix()
        );
    }
}
