//! A wall-clock ceiling on every request.
//!
//! # Why not `tower_http::timeout`
//!
//! Because it answers with a bare 408 and an empty body, which breaks the
//! guarantee in [`crate::error`] that every failure is the same envelope. The
//! logic is four lines; the envelope is the product.
//!
//! # Why 504 and not 408
//!
//! 408 means "the *client* took too long to send its request". Here the client
//! sent everything fine and *we* took too long, which is 504's exact meaning. The
//! distinction is not pedantry: many HTTP clients and proxies auto-retry a 408
//! immediately, and a payments endpoint that invites an unconditional retry
//! after an unknown outcome is how a timeout becomes a double charge.
//!
//! # What a timeout does and does not undo
//!
//! Dropping the handler future cancels the request's work at the next await
//! point and returns its database connection to the pool. It does **not** roll
//! back anything already committed — `engine` commits atomically, so the outcome
//! is either fully applied or not applied at all, but which of the two happened
//! is genuinely unknown at this point. That is why the 504 message points the
//! caller at their idempotency key: replaying it is the only way to find out.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::error;

use crate::error::ApiError;
use crate::state::AppState;

pub async fn enforce(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let budget = state.config.request_timeout;
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    match tokio::time::timeout(budget, next.run(req)).await {
        Ok(response) => response,
        Err(_elapsed) => {
            // Always worth an error line: a request hitting the ceiling means
            // something downstream is wedged, and the caller's 504 is the only
            // other trace it will leave.
            error!(%method, %path, ?budget, "request exceeded its time budget");
            ApiError::timeout().into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiConfig;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use serde_json::Value;
    use std::time::Duration;
    use tower::ServiceExt;

    /// The timeout path is the only middleware here that needs no database, so
    /// it is exercised against a state built purely from configuration.
    fn app(budget: Duration, handler_delay: Duration) -> Router {
        let state = AppState::for_timeout_tests(ApiConfig {
            request_timeout: budget,
            ..ApiConfig::for_tests()
        });

        Router::new()
            .route(
                "/",
                get(move || async move {
                    tokio::time::sleep(handler_delay).await;
                    "done"
                }),
            )
            .layer(axum::middleware::from_fn_with_state(state.clone(), enforce))
            .with_state(state)
    }

    async fn call(budget: Duration, delay: Duration) -> (StatusCode, Value) {
        let res = app(budget, delay)
            .oneshot(axum::http::Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_request_inside_the_budget_is_untouched() {
        let (status, _) = call(Duration::from_secs(5), Duration::ZERO).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_overrunning_request_becomes_a_504_in_the_envelope() {
        let (status, body) = call(Duration::from_millis(20), Duration::from_secs(30)).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["error"]["code"], "request_timeout");
        // The caller must be told how to discover what actually happened.
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Idempotency-Key"));
    }
}
