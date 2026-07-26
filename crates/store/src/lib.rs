#![forbid(unsafe_code)]

pub mod error;
pub mod pool;
pub mod types;

pub use error::{StoreError, StoreResult};