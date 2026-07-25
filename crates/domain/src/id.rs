use core::fmt;
use serde::{Deserialize, Serialize};
use std::{marker::PhantomData, str::FromStr};
use thiserror::Error;
use uuid::Uuid;

pub struct Id<T> {
    uuid: Uuid,
    _marker: PhantomData<T>,
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Id<T> {}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl<T> Eq for Id<T> {}

impl<T> std::hash::Hash for Id<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl<T> Id<T> {
    pub fn new() -> Self {
        Id {
            uuid: Uuid::now_v7(),
            _marker: PhantomData,
        }
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            _marker: PhantomData,
        }
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn into_uuid(self) -> Uuid {
        self.uuid
    }
}

impl<T> Default for Id<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Associates a short human-readable prefix with a marker type.
/// Implement this on the marker type to get `"pi_01H8..."` display format.
pub trait Prefixed {
    const PREFIX: &'static str;
}

impl<T: Prefixed> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}_{}", T::PREFIX, self.uuid)
    }
}

impl<T: Prefixed> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdParseError {
    #[error("expected prefix '{expected}_', found '{got}'")]
    WrongPrefix { expected: &'static str, got: String },

    #[error("invalid UUID portion")]
    InvalidUuid,
}

impl<T: Prefixed> FromStr for Id<T> {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let prefix = T::PREFIX;

        let rest = s
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix("_"))
            .ok_or_else(|| IdParseError::WrongPrefix {
                expected: prefix,
                got: s.to_owned(),
            })?;

        let uuid = Uuid::parse_str(rest).map_err(|_| IdParseError::InvalidUuid)?;

        Ok(Self::from_uuid(uuid))
    }
}

impl<T: Prefixed> Serialize for Id<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de, T: Prefixed> Deserialize<'de> for Id<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Marker types live next to their domain structs. Define them all here so
/// there is exactly one place to look for ID prefixes.
pub struct PaymentIntentMarker;

impl Prefixed for PaymentIntentMarker {
    const PREFIX: &'static str = "pi";
}

pub struct PaymentMethodMarker;

impl Prefixed for PaymentMethodMarker {
    const PREFIX: &'static str = "pm";
}

pub struct CustomerMarker;

impl Prefixed for CustomerMarker {
    const PREFIX: &'static str = "cus";
}

pub struct RefundMarker;

impl Prefixed for RefundMarker {
    const PREFIX: &'static str = "re";
}

pub struct DisputeMarker;

impl Prefixed for DisputeMarker {
    const PREFIX: &'static str = "dp";
}

pub struct MerchantMarker;

impl Prefixed for MerchantMarker {
    const PREFIX: &'static str = "mer";
}

pub struct ApiKeyMarker;

impl Prefixed for ApiKeyMarker {
    const PREFIX: &'static str = "key";
}

pub struct EventMarker;

impl Prefixed for EventMarker {
    const PREFIX: &'static str = "evt";
}

pub struct WebhookEndpointMarker;

impl Prefixed for WebhookEndpointMarker {
    const PREFIX: &'static str = "we";
}

pub struct SettlementMarker;

impl Prefixed for SettlementMarker {
    const PREFIX: &'static str = "set";
}

pub struct LedgerTransactionMarker;

impl Prefixed for LedgerTransactionMarker {
    const PREFIX: &'static str = "ltx";
}
pub struct JobMarker;

impl Prefixed for JobMarker {
    const PREFIX: &'static str = "job";
}

pub struct OrderMarker;
impl Prefixed for OrderMarker {
    const PREFIX: &'static str = "order";
}
pub type OrderId = Id<OrderMarker>;

pub struct PaymentMarker;
impl Prefixed for PaymentMarker {
    const PREFIX: &'static str = "pay";
}
pub type PaymentId = Id<PaymentMarker>;

pub type PaymentIntentId = Id<PaymentIntentMarker>;
pub type PaymentMethodId = Id<PaymentMethodMarker>;
pub type CustomerId = Id<CustomerMarker>;
pub type RefundId = Id<RefundMarker>;
pub type DisputeId = Id<DisputeMarker>;
pub type MerchantId = Id<MerchantMarker>;
pub type ApiKeyId = Id<ApiKeyMarker>;
pub type EventId = Id<EventMarker>;
pub type WebhookEndpointId = Id<WebhookEndpointMarker>;
pub type SettlementId = Id<SettlementMarker>;
pub type LedgerTransactionId = Id<LedgerTransactionMarker>;
pub type JobId = Id<JobMarker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_copy() {
        let a = PaymentIntentId::new();
        let b = a; // Copy — `a` still usable
        assert_eq!(a, b);
    }

    #[test]
    fn two_fresh_ids_are_not_equal() {
        assert_ne!(PaymentIntentId::new(), PaymentIntentId::new());
    }

    #[test]
    fn display_uses_correct_prefix() {
        assert!(PaymentIntentId::new().to_string().starts_with("pi_"));
        assert!(RefundId::new().to_string().starts_with("re_"));
        assert!(CustomerId::new().to_string().starts_with("cus_"));
    }

    #[test]
    fn wrong_prefix_is_rejected() {
        let re_id = RefundId::new().to_string(); // "re_..."
        println!("{} <========re_id", re_id);
        let result: Result<PaymentIntentId, _> = re_id.parse();
        assert!(matches!(result, Err(IdParseError::WrongPrefix { .. })));
    }
}
