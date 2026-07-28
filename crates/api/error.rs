//! The public error envelope — the ONLY error shape the API ever returns:
//!
//! ```json
//! {
//!   "error": {
//!     "type": "invalid_request_error",
//!     "code": "currency_invalid",
//!     "message": "'XYZ' is not a supported currency.",
//!     "param": "currency",
//!     "request_id": "req_0192f3..."
//!   }
//! }
//! ```
//!
//! "Only" is a strong claim, and it is enforced rather than hoped for. Axum's
//! own rejections (bad JSON, missing content type, oversized body) return plain
//! text by default, so [`crate::extract`] wraps every extractor and converts
//! them here. Unmatched paths and methods reach [`crate::routes`]'s fallbacks.
//! Panics reach the catch-panic layer. A client can therefore write one error
//! parser and be done — which is the entire reason merchants find some gateways
//! pleasant to integrate and others not.
//!
//! Internal errors (store failures, ledger violations) map to a generic 500;
//! their details go to logs, never to the wire. Leaking internals is both a
//! security issue and an API-stability trap — merchants WILL string-match
//! whatever you send them.
//!
//! # Choosing a status
//!
//! The distinction that matters most is *whose problem is it, and should they
//! retry*: 4xx means "change something and try again", 503/504 mean "change
//! nothing, try again shortly", 500 means "we will fix it, retrying may not
//! help". Collapsing an outage into a 401 or a 400 is the classic mistake —
//! see [`unavailable`](ApiError::unavailable).

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use engine::EngineError;
use serde_json::json;
use store::StoreError;
use tracing::error;

use crate::middleware::request_id;

/// Shorthand for the return type every handler in this crate uses.
pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub err_type: &'static str,
    pub code: String,
    pub message: String,
    /// Which input field the caller should look at, when we can name one.
    pub param: Option<String>,
    /// Set on card errors when a (failed) payment row exists, so the merchant
    /// can fetch it. Mirrors how real gateways attach the object to a decline.
    pub payment_id: Option<String>,
    /// Seconds to wait before retrying, emitted as `Retry-After`. Only set on
    /// statuses where retrying is the correct response.
    pub retry_after_secs: Option<u32>,
}

impl ApiError {
    /// The base constructor. Prefer one of the named helpers below; they exist
    /// so that the `type`/`status` pairing is decided in one place per class of
    /// error rather than at every call site.
    fn new(
        status: StatusCode,
        err_type: &'static str,
        code: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            err_type,
            code: code.into(),
            message: message.into(),
            param: None,
            payment_id: None,
            retry_after_secs: None,
        }
    }

    /// Name the offending input field. Used by the extractors, which learn the
    /// path (`card.exp_month`) only after serde has failed on it.
    #[must_use]
    pub fn with_param(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    /// Advertise `Retry-After`. Only meaningful where retrying is genuinely the
    /// right move — a 409 caused by the caller's own in-flight request, say.
    /// Setting it on a validation failure would be an instruction to hammer us.
    #[must_use]
    pub fn retry_after(mut self, secs: u32) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    // -- 4xx: the caller can fix it ----------------------------------------

    pub fn invalid_request(code: &str, message: impl Into<String>, param: Option<&str>) -> Self {
        Self {
            param: param.map(str::to_owned),
            ..Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                code,
                message,
            )
        }
    }

    pub fn not_found(resource: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "resource_missing",
            format!("No such {resource}."),
        )
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            message,
        )
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "permission_error",
            "insufficient_scope",
            message,
        )
    }

    /// 404 for a path that is not an endpoint at all, as opposed to an endpoint
    /// whose resource is missing. Both are 404s — HTTP has one status for the
    /// two — but the `code` distinguishes them, which is the difference between
    /// "your URL is wrong" and "that order does not exist".
    pub fn unknown_endpoint(method: &str, path: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "unknown_endpoint",
            format!("{method} {path} is not an endpoint on this API."),
        )
    }

    /// 405 — the path exists, this method does not. Distinct from a 404 so a
    /// caller can tell "I got the URL wrong" from "I got the verb wrong".
    pub fn method_not_allowed(method: &str, path: &str) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_request_error",
            "method_not_allowed",
            format!("{method} is not supported on {path}."),
        )
    }

    /// 409 — the request conflicts with state that already exists. Idempotency
    /// key reuse is the main producer.
    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "idempotency_error", code, message)
    }

    /// 413 — over [`ApiConfig::max_body_bytes`](crate::ApiConfig::max_body_bytes).
    ///
    /// `limit` is optional because the two places that raise this know different
    /// amounts: the idempotency middleware holds the configuration and can name
    /// the number, while a generic extractor only learns that some limit was
    /// hit. Naming a limit we are guessing at is worse than not naming one.
    pub fn payload_too_large(limit: Option<usize>) -> Self {
        let message = match limit {
            Some(bytes) => format!("Request body exceeds the {bytes} byte limit."),
            None => "Request body is too large.".to_owned(),
        };
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            message,
        )
    }

    /// 415 — we only speak JSON, and the caller did not say they were sending
    /// it. A surprising number of integration bugs are exactly this.
    pub fn unsupported_media_type() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "unsupported_media_type",
            "Send 'Content-Type: application/json'.",
        )
    }

    /// 402 — the card itself is the problem. `payment_id` present when a
    /// failed payment row was committed before this error was raised.
    pub fn card(code: &str, message: impl Into<String>, payment_id: Option<String>) -> Self {
        Self {
            payment_id,
            ..Self::new(StatusCode::PAYMENT_REQUIRED, "card_error", code, message)
        }
    }

    // -- 5xx: ours ----------------------------------------------------------

    /// 503 — a dependency we need to answer the request was down or too slow.
    /// Distinct from [`internal`](Self::internal) on purpose: this one tells the
    /// caller the request never happened and is safe to retry. Never reuse an
    /// authentication or validation status for an outage — a merchant told
    /// "invalid API key" during a database blip will rotate credentials in the
    /// middle of an incident.
    pub fn unavailable() -> Self {
        Self {
            retry_after_secs: Some(1),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "service_unavailable",
                "The service is temporarily unavailable. Please retry.",
            )
        }
    }

    /// 504 — we gave up waiting on ourselves. The caller must treat the outcome
    /// as *unknown*, not as failed: the work may have committed after we stopped
    /// waiting. This is precisely the case idempotency keys exist for, and the
    /// message says so.
    pub fn timeout() -> Self {
        Self {
            retry_after_secs: Some(1),
            ..Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "request_timeout",
                "The request took too long and was abandoned. Retry with the same \
                 Idempotency-Key to find out whether it completed.",
            )
        }
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "internal_error",
            "An internal error occurred.",
        )
    }

    /// A 500 that logs `err` first. The single approved way to discard error
    /// detail: `map_err(|_| ApiError::internal())` throws away the only copy of
    /// what actually went wrong, and the on-call engineer is left with a
    /// timestamp and the word "internal".
    pub fn internal_from(err: impl std::fmt::Debug, context: &'static str) -> Self {
        error!(error = ?err, context, "unhandled internal error");
        Self::internal()
    }

    /// Whether this error is worth an `error!` line when it leaves the server.
    /// 4xx are the caller's business and would otherwise drown the logs.
    #[must_use]
    pub fn is_server_fault(&self) -> bool {
        self.status.is_server_error()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut err = json!({
            "type": self.err_type,
            "code": self.code,
            "message": self.message,
            "param": self.param,
        });
        if let Some(pid) = &self.payment_id {
            err["payment_id"] = json!(pid);
        }
        // Read rather than threaded: see `middleware::request_id` for why.
        if let Some(id) = request_id::current() {
            err["request_id"] = json!(id.as_str());
        }

        let mut response = (self.status, Json(json!({ "error": err }))).into_response();

        if let Some(secs) = self.retry_after_secs {
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
        }
        response
    }
}

impl From<StoreError> for ApiError {
    /// Persistence failures. Only "not found" and "conflict" carry information
    /// the caller can act on; everything else is a bug or an outage and is
    /// logged, not described.
    fn from(e: StoreError) -> Self {
        match &e {
            StoreError::NotFound { resource } => ApiError::not_found(resource),
            StoreError::Conflict { .. } => {
                ApiError::conflict("resource_already_exists", "That resource already exists.")
            }
            other => ApiError::internal_from(other, "store"),
        }
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::NotFound { resource } => ApiError::not_found(resource),

            // Persistence failures keep the mapping they already have rather
            // than collapsing into a 500 on the way through the engine. A
            // duplicate `receipt` is a unique violation the caller can fix by
            // sending a different one; reporting it as an internal error would
            // tell them, wrongly, that the fault is ours and that retrying the
            // same request might work.
            EngineError::Store(s) => ApiError::from(s),

            EngineError::Validation { field, reason } => ApiError::invalid_request(
                "parameter_invalid",
                format!("{field}: {reason}"),
                Some(field),
            ),

            // Card metadata problems (Luhn, expiry, bad month) are 402s — the
            // card is the problem, not the request shape.
            EngineError::Card(c) => ApiError::card("card_invalid", c.to_string(), None),

            // Illegal transitions read as invalid requests with the transition
            // message ("cannot transition order from 'Paid' to 'Attempted'").
            EngineError::OrderTransition(t) => {
                ApiError::invalid_request("invalid_state", t.to_string(), None)
            }
            EngineError::PaymentTransition(t) => {
                ApiError::invalid_request("invalid_state", t.to_string(), None)
            }

            EngineError::Money(m) => {
                ApiError::invalid_request("amount_invalid", m.to_string(), Some("amount"))
            }

            // Everything else is internal: log the detail, return the generic.
            other => ApiError::internal_from(other, "engine"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::RETRY_AFTER;

    async fn body_of(err: ApiError) -> serde_json::Value {
        let res = err.into_response();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn every_error_carries_the_four_envelope_fields() {
        for err in [
            ApiError::invalid_request("c", "m", Some("p")),
            ApiError::not_found("order"),
            ApiError::unauthorized("m"),
            ApiError::forbidden("m"),
            ApiError::conflict("c", "m"),
            ApiError::card("declined", "m", None),
            ApiError::unavailable(),
            ApiError::timeout(),
            ApiError::internal(),
            ApiError::payload_too_large(Some(1024)),
            ApiError::payload_too_large(None),
            ApiError::unsupported_media_type(),
            ApiError::method_not_allowed("PUT", "/v1/orders"),
        ] {
            let body = body_of(err).await;
            let e = &body["error"];
            assert!(e["type"].is_string(), "{body}");
            assert!(e["code"].is_string(), "{body}");
            assert!(e["message"].is_string(), "{body}");
            assert!(
                e.get("param").is_some(),
                "param key must always exist: {body}"
            );
            assert_eq!(body.as_object().unwrap().len(), 1, "error is the only key");
        }
    }

    #[tokio::test]
    async fn payment_id_appears_only_on_declines_that_have_one() {
        let with = body_of(ApiError::card("declined", "m", Some("pay_1".into()))).await;
        assert_eq!(with["error"]["payment_id"], "pay_1");

        let without = body_of(ApiError::card("declined", "m", None)).await;
        assert!(without["error"].get("payment_id").is_none());
    }

    #[tokio::test]
    async fn retryable_errors_advertise_retry_after() {
        for err in [ApiError::unavailable(), ApiError::timeout()] {
            let res = err.into_response();
            assert!(
                res.headers().contains_key(RETRY_AFTER),
                "missing Retry-After"
            );
        }
        // A validation failure is not retryable and must not suggest otherwise.
        let res = ApiError::invalid_request("c", "m", None).into_response();
        assert!(!res.headers().contains_key(RETRY_AFTER));
    }

    #[tokio::test]
    async fn request_id_is_absent_outside_a_request() {
        // No middleware in scope, so no id — and crucially, no panic.
        let body = body_of(ApiError::internal()).await;
        assert!(body["error"].get("request_id").is_none());
    }

    #[test]
    fn statuses_match_their_semantics() {
        assert_eq!(
            ApiError::unavailable().status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(ApiError::timeout().status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(ApiError::conflict("c", "m").status, StatusCode::CONFLICT);
        assert!(ApiError::internal().is_server_fault());
        assert!(!ApiError::not_found("order").is_server_fault());
    }

    #[test]
    fn store_not_found_becomes_a_404_and_everything_else_stays_opaque() {
        let e: ApiError = StoreError::not_found("order").into();
        assert_eq!(e.status, StatusCode::NOT_FOUND);

        let e: ApiError = StoreError::LedgerIntegrity("debits != credits".into()).into();
        assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !e.message.contains("debits"),
            "internal detail leaked to the wire: {}",
            e.message
        );
    }

    #[test]
    fn engine_validation_names_the_field_in_param() {
        let e: ApiError = EngineError::validation("amount", "must be at least 100").into();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert_eq!(e.param.as_deref(), Some("amount"));
        assert!(e.message.contains("must be at least 100"));
    }

    #[test]
    fn cross_merchant_engine_not_found_stays_a_404() {
        let e: ApiError = EngineError::not_found("order").into();
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert_eq!(e.code, "resource_missing");
    }
}
