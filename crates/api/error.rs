//! The public error envelope — the ONLY error shape the API ever returns:
//!
//! ```json
//! { "error": { "type": "...", "code": "...", "message": "...", "param": null } }
//! ```
//!
//! Internal errors (store failures, ledger violations) map to a generic 500;
//! their details go to logs, never to the wire. Leaking internals is both a
//! security issue and an API-stability trap — merchants WILL string-match
//! whatever you send them.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use engine::EngineError;
use serde_json::json;
use tracing::error;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub err_type: &'static str,
    pub code: String,
    pub message: String,
    pub param: Option<&'static str>,
    /// Set on card errors when a (failed) payment row exists, so the merchant
    /// can fetch it. Mirrors how real gateways attach the object to a decline.
    pub payment_id: Option<String>,
}

impl ApiError {
    pub fn invalid_request(
        code: &str,
        message: impl Into<String>,
        param: Option<&'static str>,
    ) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err_type: "invalid_request_error",
            code: code.into(),
            message: message.into(),
            param,
            payment_id: None,
        }
    }

    pub fn not_found(resource: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            err_type: "invalid_request_error",
            code: "resource_missing".into(),
            message: format!("No such {resource}."),
            param: None,
            payment_id: None,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            err_type: "authentication_error",
            code: "invalid_api_key".into(),
            message: message.into(),
            param: None,
            payment_id: None,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            err_type: "permission_error",
            code: "insufficient_scope".into(),
            message: message.into(),
            param: None,
            payment_id: None,
        }
    }

    /// 402 — the card itself is the problem. `payment_id` present when a
    /// failed payment row was committed before this error was raised.
    pub fn card(code: &str, message: impl Into<String>, payment_id: Option<String>) -> Self {
        Self {
            status: StatusCode::PAYMENT_REQUIRED,
            err_type: "card_error",
            code: code.into(),
            message: message.into(),
            param: None,
            payment_id,
        }
    }

    /// 503 — a dependency we need to answer the request was down or too slow.
    /// Distinct from [`internal`](Self::internal) on purpose: this one tells the
    /// caller the request never happened and is safe to retry. Never reuse an
    /// authentication or validation status for an outage — a merchant told
    /// "invalid API key" during a database blip will rotate credentials in the
    /// middle of an incident.
    pub fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            err_type: "api_error",
            code: "service_unavailable".into(),
            message: "The service is temporarily unavailable. Please retry.".into(),
            param: None,
            payment_id: None,
        }
    }

    pub fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err_type: "api_error",
            code: "internal_error".into(),
            message: "An internal error occurred.".into(),
            param: None,
            payment_id: None,
        }
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
        (self.status, Json(json!({ "error": err }))).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match &e {
            EngineError::NotFound { resource } => ApiError::not_found(resource),
            EngineError::Store(s) if s.is_not_found() => ApiError::not_found("resource"),

            EngineError::Validation { field, reason } => {
                let mut err = ApiError::invalid_request("parameter_invalid", reason.clone(), None);
                err.message = format!("{field}: {reason}");
                err
            }

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
            other => {
                error!(error = ?other, "internal engine error");
                ApiError::internal()
            }
        }
    }
}
