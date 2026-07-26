use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::id::MerchantId;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MerchantError {
    #[error("merchant name must not be empty")]
    EmptyName,

    #[error("invalid email address: {0}")]
    InvalidEmail(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merchant {
    pub id: MerchantId,
    pub name: String,
    pub email: String,
    /// Origins allowed to embed the hosted checkout iframe / receive
    /// postMessage handoffs. Enforced via CSP `frame-ancestors` and origin
    /// checks on both sides of the postMessage channel — see the checkout
    /// milestone. Empty until the merchant registers a domain.
    pub checkout_allowed_domains: Vec<String>,
    pub created_at: OffsetDateTime,
}

impl Merchant {
    pub fn new(
        name: impl Into<String>,
        email: impl Into<String>,
        now: OffsetDateTime,
    ) -> Result<Self, MerchantError> {
        let name = name.into();
        let email = email.into();

        if name.trim().is_empty() {
            return Err(MerchantError::EmptyName);
        }
        if !is_plausible_email(&email) {
            return Err(MerchantError::InvalidEmail(email));
        }

        Ok(Self {
            id: MerchantId::new(),
            name,
            email,
            checkout_allowed_domains: Vec::new(),
            created_at: now,
        })
    }

    /// Add a domain to the checkout allowlist. Idempotent — adding the same
    /// domain twice is a no-op rather than a duplicate entry.
    pub fn allow_checkout_domain(&mut self, domain: impl Into<String>) {
        let domain = domain.into();
        if !self.checkout_allowed_domains.contains(&domain) {
            self.checkout_allowed_domains.push(domain);
        }
    }

    /// Whether `origin` (e.g. `"https://shop.example.com"`) is permitted to
    /// embed this merchant's checkout. Used by the CSP header and the
    /// postMessage origin check in the checkout milestone.
    pub fn allows_checkout_origin(&self, origin: &str) -> bool {
        self.checkout_allowed_domains.iter().any(|d| d == origin)
    }
}

/// Deliberately permissive — this is a sandbox, not an email verification
/// service. It exists to catch obvious typos (`"bob"`, `""`), not to be a
/// correct RFC 5322 validator.
fn is_plausible_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn now() -> OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    #[test]
    fn valid_merchant_constructs() {
        let m = Merchant::new("Acme Co", "billing@acme.dev", now()).unwrap();
        assert_eq!(m.name, "Acme Co");
        assert!(m.checkout_allowed_domains.is_empty());
    }

    #[test]
    fn empty_name_rejected() {
        assert_eq!(
            Merchant::new("   ", "billing@acme.dev", now()),
            Err(MerchantError::EmptyName)
        );
    }

    #[test]
    fn bad_email_rejected() {
        assert!(Merchant::new("Acme", "not-an-email", now()).is_err());
        assert!(Merchant::new("Acme", "a@b", now()).is_err());
        assert!(Merchant::new("Acme", "@missing-local.com", now()).is_err());
    }

    #[test]
    fn allowlist_is_idempotent() {
        let mut m = Merchant::new("Acme", "b@acme.dev", now()).unwrap();
        m.allow_checkout_domain("https://shop.acme.dev");
        m.allow_checkout_domain("https://shop.acme.dev");
        assert_eq!(m.checkout_allowed_domains.len(), 1);
    }

    #[test]
    fn origin_check_matches_exactly() {
        let mut m = Merchant::new("Acme", "b@acme.dev", now()).unwrap();
        m.allow_checkout_domain("https://shop.acme.dev");
        assert!(m.allows_checkout_origin("https://shop.acme.dev"));
        assert!(!m.allows_checkout_origin("https://evil.dev"));
        assert!(!m.allows_checkout_origin("http://shop.acme.dev")); // scheme matters
    }
}
