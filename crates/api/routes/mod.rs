//! Route assembly and the middleware stack.
//!
//! Everything about *which* endpoints exist lives in the sibling modules; this
//! file decides how a request reaches them and what wraps it on the way. It is
//! the one place in the crate where ordering is load-bearing, so the reasoning
//! is written down rather than left to be reverse-engineered from `.layer()`
//! call order.
//!
//! # Reading the stack
//!
//! `tower` applies layers inside-out: the **last** `.layer()` in a chain is the
//! **outermost**, and therefore the first to see a request and the last to touch
//! a response. The chain in [`router`] is written innermost-first and annotated
//! with why each layer sits where it does.
//!
//! # The invariants worth protecting
//!
//! * **Nothing escapes the error envelope.** Unmatched paths, unmatched methods,
//!   rejected extractors and panics all funnel into [`crate::error::ApiError`].
//! * **Every response carries a request id**, including the ones produced by
//!   layers below the router, which is why request-id sits outside almost
//!   everything.
//! * **Idempotency runs inside authentication.** Keys are scoped per merchant,
//!   so the middleware needs an authenticated caller; layering it outside auth
//!   would let an unauthenticated request occupy a key.
//! * **`/health` is never authenticated**, and is deliberately mounted on the
//!   root router rather than under `/v1` — an orchestrator probing readiness has
//!   no API key, and a version prefix on a probe URL is a migration hazard.

pub mod events;
pub mod health;
pub mod orders;
pub mod payments;
pub mod webhook_endpoints;

use std::any::Any;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, RETRY_AFTER};
use axum::http::{Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{Any as AnyOrigin, CorsLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::error::ApiError;
use crate::middleware::{idempotency, request_id, timeout};
use crate::state::AppState;

/// Build the application router.
///
/// Returns a `Router` with state already applied, so callers can both serve it
/// and drive it directly with `tower::ServiceExt::oneshot` in tests — which is
/// why no layer here is allowed to change the return type.
pub fn router(state: Arc<AppState>) -> Router {
    let cors = cors_layer();

    // --- /v1: everything that needs an API key ------------------------------
    let authed = Router::new()
        .merge(orders::routes())
        .merge(payments::routes())
        .merge(webhook_endpoints::routes())
        .merge(events::routes())
        // Fallbacks are declared before the layers so that a 404 or 405 under
        // /v1 still passes through auth — an unauthenticated caller learns
        // "unauthorized", never which paths happen to exist.
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // Inside auth: the key is scoped to a merchant and the middleware
        // needs `AuthCtx` from the request extensions.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            idempotency::enforce,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    Router::new()
        .merge(health::routes())
        .nest("/v1", authed)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // --- innermost: bound the work a single request can cause ------------
        // Ahead of every extractor, so an oversized body is refused before it
        // is buffered rather than after.
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes))
        // Bounds *time* the way the body limit bounds bytes. Inside tracing so
        // the abandoned request is still recorded with its latency.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            timeout::enforce,
        ))
        .layer(TraceLayer::new_for_http())
        // Outside tracing so a panic is logged as a failed request, and inside
        // request-id so the 500 it produces still carries one.
        .layer(CatchPanicLayer::custom(handle_panic))
        // Outside almost everything: layers below can fail, and their responses
        // must be correlatable too.
        .layer(axum::middleware::from_fn(request_id::propagate))
        // Must be outside `TraceLayer` to have any effect — it marks headers
        // sensitive *before* the tracing layer reads them, which is what keeps
        // API keys out of the logs.
        .layer(SetSensitiveRequestHeadersLayer::new([
            AUTHORIZATION,
            COOKIE,
        ]))
        // Outermost, so even a rejected preflight or an error response carries
        // the CORS headers a browser needs to read it.
        .layer(cors)
        .with_state(state)
}

/// Browser access policy.
///
/// Any origin, no credentials. That combination is safe *because* this API
/// authenticates with a bearer token and never a cookie: with
/// `allow_credentials(false)` the browser will not attach ambient
/// authentication, so a hostile page can only make calls with a key it already
/// has — in which case it does not need the browser. Allowing credentials
/// alongside a wildcard origin is the actual mistake, and the CORS spec forbids
/// that pairing outright.
///
/// Secret keys still belong on a server. The permissive policy exists for the
/// sandbox dashboard and for `fetch` from a local page during integration.
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AnyOrigin)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            AUTHORIZATION,
            CONTENT_TYPE,
            idempotency::HEADER,
            request_id::HEADER,
        ])
        // Response headers are hidden from JavaScript unless exposed. These
        // three are the ones a client actually has to act on.
        .expose_headers([
            request_id::HEADER,
            idempotency::REPLAYED_HEADER,
            RETRY_AFTER,
        ])
        .max_age(std::time::Duration::from_secs(600))
}

/// Unmatched path.
///
/// A wrong URL is the single most common integration mistake, so the response
/// names the method and path rather than being an empty 404 — and it is in the
/// standard envelope, so a client's error parser handles it like any other
/// failure.
async fn not_found(method: Method, uri: Uri) -> ApiError {
    ApiError::unknown_endpoint(method.as_str(), uri.path())
}

/// Matched path, wrong verb. Distinguished from a 404 so a caller can tell
/// "I typed the URL wrong" from "I used the wrong method".
async fn method_not_allowed(method: Method, uri: Uri) -> ApiError {
    ApiError::method_not_allowed(method.as_str(), uri.path())
}

/// Turn a panic into the error envelope instead of a dropped connection.
///
/// Without this, a panic in a handler kills the connection with no response at
/// all: the client sees a transport error and cannot tell a bug from a network
/// fault — and for a payment request, "no answer" is the worst possible answer.
/// The panic message is logged and never sent; it routinely contains internal
/// paths and values.
fn handle_panic(panic: Box<dyn Any + Send + 'static>) -> Response {
    let detail = panic
        .downcast_ref::<&'static str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned());

    tracing::error!(panic = %detail, "handler panicked");
    ApiError::internal().into_response()
}

/// RFC3339 rendering shared by the route modules.
///
/// Timestamps that fail to format would otherwise each get their own
/// `unwrap_or_default()` and silently become an empty string; one helper means
/// one decision. The fallback cannot be reached with a well-formed
/// `OffsetDateTime`, so it signals a bug loudly rather than looking like a date.
pub(crate) fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_else(|err| {
        tracing::error!(error = %err, "timestamp could not be formatted");
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A router with no state and no database, exercising only the pieces that
    /// do not need one.
    fn bare() -> Router {
        Router::new()
            .route("/thing", axum::routing::get(|| async { "ok" }))
            .route(
                "/boom",
                axum::routing::get(|| async {
                    panic!("internal detail: secret_value");
                    #[allow(unreachable_code)]
                    ""
                }),
            )
            .fallback(not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .layer(CatchPanicLayer::custom(handle_panic))
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn an_unknown_path_is_a_404_in_the_envelope() {
        let response = bare()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "unknown_endpoint");
        assert!(body["error"]["message"].as_str().unwrap().contains("/nope"));
    }

    #[tokio::test]
    async fn a_wrong_method_is_a_405_not_a_404() {
        let response = bare()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/thing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "method_not_allowed");
    }

    #[tokio::test]
    async fn a_panic_becomes_a_500_that_leaks_nothing() {
        let response = bare()
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "api_error");
        assert!(
            !body.to_string().contains("secret_value"),
            "panic detail reached the wire: {body}"
        );
    }

    #[test]
    fn timestamps_render_as_rfc3339() {
        assert_eq!(
            rfc3339(time::macros::datetime!(2026-01-01 12:30:00 UTC)),
            "2026-01-01T12:30:00Z"
        );
    }
}
