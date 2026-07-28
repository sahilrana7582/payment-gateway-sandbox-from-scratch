//! The HTTP layer: transport concerns only.
//!
//! Everything here is about turning bytes on a socket into a call into `engine`
//! and back again — parsing, authenticating, bounding, correlating, and
//! rendering. No business rule lives in this crate. If a check would still be
//! true for a caller arriving over gRPC or a queue, it belongs in `engine` or
//! `domain` instead, and a rule implemented in both places will drift.
//!
//! # Layout
//!
//! | module | responsibility |
//! |---|---|
//! | [`routes`] | endpoints, request DTOs, and the middleware stack |
//! | [`middleware`] | request id, idempotency, timeouts |
//! | [`auth`] | API-key authentication, scope checks, the auth cache |
//! | [`error`] | the single public error envelope |
//! | [`extract`] | extractors that fail *into* that envelope |
//! | [`pagination`] | cursor paging shared by every collection |
//! | [`validate`] | edge-only validation (URLs, header shapes, sizes) |
//! | [`config`] | every operational limit, in one struct |
//! | [`state`] | what handlers share |

#![forbid(unsafe_code)]
// `ApiError` carries three owned strings so an error can name the offending
// parameter and say something useful, which puts it over clippy's size
// threshold for a `Result` error variant. Boxing it would shrink the `Ok` path
// of every handler by a few bytes and cost an allocation on every failure, plus
// a `Box` in the signature of every constructor — a bad trade for a type that
// only ever exists on the cold path of a request that is already doing I/O.
#![allow(clippy::result_large_err)]

pub mod auth;
pub mod config;
pub mod error;
pub mod extract;
pub mod middleware;
pub mod pagination;
pub mod routes;
pub mod state;
pub mod validate;

pub use config::ApiConfig;
pub use error::{ApiError, ApiResult};
pub use routes::router;
pub use state::AppState;
