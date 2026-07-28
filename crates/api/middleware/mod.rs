//! Cross-cutting request handling.
//!
//! Each module here does one thing to every request, and the order they compose
//! in is decided in [`crate::routes::router`] — deliberately in one place, with
//! the reasoning written down, because "which layer sees the request first" is
//! the kind of detail that is obvious while writing it and impossible to
//! reconstruct six months later.
//!
//! Anything specific to one resource belongs in that resource's route module
//! instead; a middleware that has to ask "which endpoint am I on" is usually a
//! handler wearing a disguise. The exception is [`idempotency`], which is
//! genuinely endpoint-agnostic: it keys on the path rather than branching on it.

pub mod idempotency;
pub mod request_id;
pub mod timeout;
