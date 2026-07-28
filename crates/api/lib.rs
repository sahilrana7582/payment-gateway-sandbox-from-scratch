#![forbid(unsafe_code)]

pub mod auth;
pub mod error;
pub mod handlers;
// pub mod router;
pub mod state;

// pub use router::router;
pub use state::AppState;
