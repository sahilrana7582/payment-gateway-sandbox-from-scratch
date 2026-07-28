#![forbid(unsafe_code)]

pub mod delivery;
pub mod worker;

pub use delivery::{SendOutcome, WebhookConfig};
// pub use worker::WebhookDeliveryHandler;
