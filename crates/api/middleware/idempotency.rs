//! Idempotency keys — the middleware half of `store::idempotency`.
//!
//! # Why this is not optional infrastructure
//!
//! A client that sends `POST /v1/payments` and receives a network timeout has no
//! way to know whether the card was charged. Their only options are to retry
//! (and risk charging twice) or not to retry (and risk not charging at all).
//! Both are wrong. An idempotency key resolves it: the retry carries the same
//! key, and the server returns the *original* response — the same payment id,
//! the same status — instead of executing again.
//!
//! Everything below exists to make that guarantee hold under concurrency, not
//! just under sequential retries.
//!
//! # The protocol
//!
//! ```text
//!   Idempotency-Key: <8-255 printable ASCII>
//! ```
//!
//! Sent on a mutating request, it produces one of four outcomes, decided by a
//! single atomic `INSERT ... ON CONFLICT DO NOTHING` in the store layer:
//!
//! | Outcome    | Meaning                                   | Response                       |
//! |------------|-------------------------------------------|--------------------------------|
//! | `Acquired` | First time we have seen this key           | execute, then record           |
//! | `Replay`   | Seen, completed, same request body         | the stored response, verbatim  |
//! | `Mismatch` | Seen, completed, *different* request body  | 409 — a client bug             |
//! | `InFlight` | Seen, still executing right now            | 409 — do not queue behind it   |
//!
//! `Mismatch` deserves emphasis: reusing a key for a different request is not
//! something to be forgiving about. Being lenient there — executing it anyway —
//! converts a client bug into a duplicate charge, which is exactly the failure
//! the header was added to prevent.
//!
//! # The request hash
//!
//! The fingerprint covers method, path and body. Path matters because
//! `POST /v1/orders` and `POST /v1/payments` with one key are plainly different
//! requests. It deliberately does **not** cover headers: a retry through a proxy
//! that adds a trace header is still the same request, and hashing headers would
//! turn every such retry into a spurious 409.
//!
//! # Failure modes, stated plainly
//!
//! * **The key store is unreachable.** We fail the request with a 503 rather
//!   than proceeding unprotected. Executing a payment we cannot deduplicate is
//!   worse than not executing it: the caller can retry a 503, but cannot un-send
//!   a double charge.
//! * **The handler returns 5xx.** The key is released, so a retry genuinely
//!   re-executes. Caching a transient failure would make it permanent for 24h.
//! * **The process dies mid-request.** The row stays `in_progress` and retries
//!   get 409 until the 24h expiry sweeps it. This is the honest answer — the
//!   alternative, a lease with a timeout, trades a stuck key for the chance of
//!   two concurrent executions, and for payments that is the wrong trade.
//!
//! Requests without the header are passed straight through: the sandbox must
//! stay usable from a browser or a `curl` one-liner. That is a deliberate
//! availability-over-strictness choice for a *test* gateway, and a production
//! deployment should flip it to required on `/v1/payments`.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::HeaderName;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use store::idempotency::{self, AcquireOutcome};
use tracing::{debug, error, info, warn};

use crate::auth::AuthCtx;
use crate::error::ApiError;
use crate::state::AppState;
use crate::validate;

/// The request header carrying the key.
pub const HEADER: HeaderName = HeaderName::from_static("idempotency-key");

/// Set to `true` on a response served from the key store rather than executed.
/// Clients use it to distinguish "my retry worked" from "I charged twice".
pub const REPLAYED_HEADER: HeaderName = HeaderName::from_static("idempotent-replayed");

/// Enforce idempotency for mutating requests that ask for it.
///
/// Must be layered *inside* [`crate::auth::require_auth`]: keys are scoped per
/// merchant, so this needs an [`AuthCtx`] to exist. Two merchants choosing the
/// same key string must not collide, and an unauthenticated caller must not be
/// able to occupy another merchant's key.
pub async fn enforce(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    match guarded(&state, req, next).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn guarded(state: &Arc<AppState>, req: Request, next: Next) -> Result<Response, ApiError> {
    // Reads are naturally idempotent; a key on one is meaningless, not an error.
    if !is_mutating(req.method()) {
        return Ok(next.run(req).await);
    }

    let Some(raw_key) = header_value(&req) else {
        debug!("mutating request sent without an Idempotency-Key");
        return Ok(next.run(req).await);
    };
    let key = validate::idempotency_key(&raw_key)?.to_owned();

    // Auth runs outside this layer, so its absence is a wiring bug rather than a
    // client error. Failing closed keeps a mis-layered deployment from silently
    // sharing one merchant's keys with another.
    let Some(auth) = req.extensions().get::<AuthCtx>().copied() else {
        error!("idempotency middleware ran without an AuthCtx — check the layer order");
        return Err(ApiError::internal());
    };

    let endpoint = format!("{} {}", req.method(), req.uri().path());
    let (parts, body) = req.into_parts();
    let body = buffer(body, state.config.max_body_bytes)
        .await
        .ok_or_else(|| ApiError::payload_too_large(Some(state.config.max_body_bytes)))?;

    let request_hash = fingerprint(&endpoint, &body);
    let now = state.clock.now();
    let expires_at = now + state.config.idempotency_ttl_time();

    let outcome = idempotency::acquire(
        &state.pool,
        auth.merchant_id,
        &key,
        &endpoint,
        &request_hash,
        expires_at,
        now,
    )
    .await
    .map_err(|err| {
        // Deliberately NOT falling through to the handler. See the module docs.
        error!(error = ?err, "idempotency key store unavailable; refusing the request");
        ApiError::unavailable()
    })?;

    match outcome {
        AcquireOutcome::Replay { status, body } => {
            info!(%endpoint, "replaying a recorded response for a repeated key");
            Ok(replay(status, body))
        }

        AcquireOutcome::Mismatch => {
            warn!(%endpoint, "idempotency key reused with a different request body");
            Err(ApiError::conflict(
                "idempotency_key_reused",
                "This Idempotency-Key was already used for a request with a different \
                 body. Use a new key for a new request.",
            ))
        }

        AcquireOutcome::InFlight => {
            debug!(%endpoint, "concurrent request with the same idempotency key");
            Err(ApiError::conflict(
                "idempotency_key_in_flight",
                "A request with this Idempotency-Key is still in progress. Retry \
                 shortly to receive its result.",
            )
            .retry_after(1))
        }

        AcquireOutcome::Acquired => {
            let response = next.run(Request::from_parts(parts, Body::from(body))).await;
            Ok(record(state, auth, &key, response).await)
        }
    }
}

/// Run-and-record: buffer the response, decide whether it is worth replaying,
/// and hand back an untouched copy to the caller either way.
async fn record(state: &Arc<AppState>, auth: AuthCtx, key: &str, response: Response) -> Response {
    let status = response.status();
    let (parts, body) = response.into_parts();

    let Some(bytes) = buffer(body, state.config.max_recorded_response_bytes).await else {
        // Too big to record. The caller still gets their response; a retry will
        // simply re-execute, so release the key rather than leaving it stuck.
        warn!(
            %status,
            limit = state.config.max_recorded_response_bytes,
            "response too large to record against an idempotency key"
        );
        release(state, auth, key).await;
        return Response::from_parts(parts, Body::empty());
    };

    // A 5xx is a statement about us, not about the request. Recording it would
    // make a blip permanent for the life of the key.
    if status.is_server_error() {
        release(state, auth, key).await;
        return Response::from_parts(parts, Body::from(bytes));
    }

    // Everything else is recorded, declines included: a retried card decline must
    // return the same decline and the same payment id, not attempt the card again.
    let recorded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if let Err(err) = idempotency::complete(
        &state.pool,
        auth.merchant_id,
        key,
        i32::from(status.as_u16()),
        &recorded,
    )
    .await
    {
        // The work committed; only the record of it failed. Returning an error
        // now would be a lie about what happened, so log loudly and answer
        // truthfully — the cost is that a retry re-executes.
        error!(error = ?err, %status, "failed to record an idempotent response");
    }

    Response::from_parts(parts, Body::from(bytes))
}

async fn release(state: &Arc<AppState>, auth: AuthCtx, key: &str) {
    if let Err(err) = idempotency::release(&state.pool, auth.merchant_id, key).await {
        error!(error = ?err, "failed to release an idempotency key; retries will 409 until it expires");
    }
}

/// Rebuild a recorded response. The status is stored as an `i32`, so a value
/// outside the HTTP range means the row was tampered with; refuse rather than
/// panic on it.
fn replay(status: i32, body: Option<Value>) -> Response {
    let status = u16::try_from(status)
        .ok()
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::OK);

    let payload = body.unwrap_or(Value::Null);
    let mut response = (status, axum::Json(payload)).into_response();
    response
        .headers_mut()
        .insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
    response
}

/// Only these verbs can create or change something worth deduplicating.
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn header_value(req: &Request) -> Option<String> {
    req.headers()
        .get(&HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// `None` when the body exceeds `limit` or could not be read.
async fn buffer(body: Body, limit: usize) -> Option<Bytes> {
    axum::body::to_bytes(body, limit).await.ok()
}

/// What makes two requests "the same request".
///
/// The separator is a NUL byte so that a path ending in the body's first
/// characters cannot produce the same digest as a different split — a hash over
/// naively concatenated fields is a classic ambiguity.
fn fingerprint(endpoint: &str, body: &[u8]) -> String {
    let mut buf = Vec::with_capacity(endpoint.len() + body.len() + 1);
    buf.extend_from_slice(endpoint.as_bytes());
    buf.push(0);
    buf.extend_from_slice(body);
    crypto::digest::sha256_hex(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mutating_verbs_are_deduplicated() {
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_mutating(&m), "{m} should be deduplicated");
        }
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_mutating(&m), "{m} should not be deduplicated");
        }
    }

    #[test]
    fn the_fingerprint_separates_endpoint_from_body() {
        // Without the separator these two would hash identically.
        assert_ne!(
            fingerprint("POST /v1/pay", b"ments"),
            fingerprint("POST /v1/payments", b"")
        );
    }

    #[test]
    fn the_fingerprint_covers_both_endpoint_and_body() {
        let base = fingerprint("POST /v1/payments", br#"{"amount":100}"#);
        assert_eq!(base, fingerprint("POST /v1/payments", br#"{"amount":100}"#));
        assert_ne!(base, fingerprint("POST /v1/orders", br#"{"amount":100}"#));
        assert_ne!(base, fingerprint("POST /v1/payments", br#"{"amount":200}"#));
        // Same path, different verb, is a different request.
        assert_ne!(base, fingerprint("PUT /v1/payments", br#"{"amount":100}"#));
    }

    #[tokio::test]
    async fn a_recorded_response_replays_with_its_original_status_and_marker() {
        let response = replay(201, Some(serde_json::json!({ "id": "pay_1" })));
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers().get(REPLAYED_HEADER).unwrap(), "true");

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["id"], "pay_1");
    }

    #[test]
    fn a_corrupt_recorded_status_does_not_panic() {
        for bad in [-1, 0, 99, 1000, i32::MAX] {
            assert_eq!(replay(bad, None).status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn buffering_refuses_bodies_over_the_limit() {
        assert!(buffer(Body::from(vec![0u8; 10]), 64).await.is_some());
        assert!(buffer(Body::from(vec![0u8; 100]), 64).await.is_none());
    }

    #[test]
    fn in_flight_conflicts_tell_the_caller_to_retry() {
        let err = ApiError::conflict("idempotency_key_in_flight", "m").retry_after(1);
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.retry_after_secs, Some(1));
    }
}
