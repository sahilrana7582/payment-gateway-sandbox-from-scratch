//! Extractors that fail in our envelope instead of axum's.
//!
//! `axum::Json` rejects a malformed body with `text/plain` and a message shaped
//! nothing like the rest of the API. That is fine for an internal service and
//! unacceptable for a public one: it means a client's error handling has to
//! special-case "sometimes the error is not JSON", which they will discover in
//! production, on a Friday. These wrappers are drop-in replacements — same
//! names, same tuple shape — so handlers read identically and the envelope
//! guarantee in [`crate::error`] actually holds.
//!
//! The messages are deliberately specific ("card.exp_month: invalid type"),
//! because axum runs serde through `serde_path_to_error` and it would be a
//! waste to throw that path away. They are also bounded and sanitised: the text
//! quotes the caller's own input back at them, and unbounded reflection of
//! attacker-controlled bytes into a response is how an error message becomes a
//! payload.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::ApiError;

/// Longest fragment of a rejection message we will echo. Long enough for a
/// serde path plus its explanation, short enough that nothing interesting fits.
const MAX_ECHO: usize = 200;

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// `axum::Json` with our error envelope. Also usable as a response type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(ApiError::from)
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        match rejection {
            // Valid JSON, wrong shape: a missing field, a string where a number
            // belongs, an unknown key. The one case where we can name the field.
            JsonRejection::JsonDataError(err) => {
                let detail = strip_prefix(&err.body_text());
                let param = serde_param(&detail);
                let mut e = ApiError::invalid_request("parameter_invalid", detail, None);
                if let Some(p) = param {
                    e = e.with_param(p);
                }
                e
            }

            JsonRejection::JsonSyntaxError(_) => ApiError::invalid_request(
                "json_invalid",
                "Request body is not valid JSON.",
                None,
            ),

            JsonRejection::MissingJsonContentType(_) => ApiError::unsupported_media_type(),

            // Body could not be read: almost always the length limit, sometimes
            // a client that hung up. Trust the rejection's own status rather
            // than guessing, since axum distinguishes them correctly.
            other => match other.status() {
                StatusCode::PAYLOAD_TOO_LARGE => ApiError::payload_too_large(None),
                _ => ApiError::invalid_request(
                    "body_unreadable",
                    "Request body could not be read.",
                    None,
                ),
            },
        }
    }
}

/// Drop axum's framing so the message starts at the part the caller can act on.
fn strip_prefix(body_text: &str) -> String {
    const FRAMING: &str = "Failed to deserialize the JSON body into the target type: ";
    let trimmed = body_text.strip_prefix(FRAMING).unwrap_or(body_text);
    sanitize(trimmed)
}

/// Collapse to a single safe line and bound the length.
fn sanitize(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_ECHO)
        .collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if raw.chars().count() > MAX_ECHO {
        out.push('…');
    }
    out
}

/// Recover the field path serde reported. Messages look like
/// `card.exp_month: invalid type: string "12", expected u8`, so the path is
/// everything before the first colon — but only when it looks like a path, so a
/// message with no path does not produce a nonsense `param`.
fn serde_param(detail: &str) -> Option<String> {
    let candidate = detail.split(':').next()?.trim();
    let plausible = !candidate.is_empty()
        && candidate.len() <= 64
        && candidate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'[' | b']'));
    plausible.then(|| candidate.to_owned())
}

// ---------------------------------------------------------------------------
// Query string
// ---------------------------------------------------------------------------

/// `axum::extract::Query` with our error envelope.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(ApiError::from)
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        let detail = sanitize(&rejection.body_text());
        let param = serde_param(&detail);
        let mut e = ApiError::invalid_request(
            "parameter_invalid",
            format!("Invalid query string. {detail}"),
            None,
        );
        if let Some(p) = param {
            e = e.with_param(p);
        }
        e
    }
}

// ---------------------------------------------------------------------------
// Path segments
// ---------------------------------------------------------------------------

/// `axum::extract::Path` with our error envelope.
#[derive(Debug, Clone, Copy, Default)]
pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(ApiError::from)
    }
}

impl From<PathRejection> for ApiError {
    /// A path segment that will not deserialize describes a resource that
    /// cannot exist, so this is a 404 rather than a 400 — same reasoning as an
    /// id in the right shape that simply is not in the table. It keeps
    /// `GET /v1/orders/{anything}` answering with exactly one status, which is
    /// what stops the endpoint from being an existence oracle.
    fn from(_: PathRejection) -> Self {
        ApiError::not_found("resource")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::routing::post;
    use axum::Router;
    use serde::Deserialize;
    use serde_json::Value;
    use tower::ServiceExt;

    /// A stand-in for a real request DTO. `card` exists only so the nested
    /// serde path (`card.exp_month`) can be exercised; nothing reads it, which
    /// is the point.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Demo {
        amount: i64,
        #[serde(default)]
        #[allow(dead_code)]
        card: Option<DemoCard>,
    }

    #[derive(Debug, Deserialize)]
    struct DemoCard {
        #[allow(dead_code)]
        exp_month: u8,
    }

    fn app() -> Router {
        Router::new().route("/", post(|Json(d): Json<Demo>| async move { d.amount.to_string() }))
    }

    async fn post_body(body: &str, content_type: Option<&str>) -> (StatusCode, Value) {
        let mut builder = axum::http::Request::builder().method("POST").uri("/");
        if let Some(ct) = content_type {
            builder = builder.header(CONTENT_TYPE, ct);
        }
        let res = app()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn malformed_json_is_a_400_in_the_envelope_not_plain_text() {
        let (status, body) = post_body("{not json", Some("application/json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "json_invalid");
    }

    #[tokio::test]
    async fn a_wrongly_typed_field_names_itself_in_param() {
        let (status, body) = post_body(
            r#"{"amount": 100, "card": {"exp_month": "twelve"}}"#,
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["param"], "card.exp_month");
        assert!(
            !body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Failed to deserialize"),
            "axum framing leaked into the message"
        );
    }

    #[tokio::test]
    async fn a_missing_required_field_is_reported_by_name() {
        let (status, body) = post_body(r#"{}"#, Some("application/json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("amount"));
    }

    #[tokio::test]
    async fn an_unknown_field_is_rejected_rather_than_ignored() {
        // A typo'd parameter that silently does nothing is how a merchant ends
        // up charging the wrong amount.
        let (status, body) = post_body(
            r#"{"amount": 100, "amunt": 999}"#,
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("amunt"));
    }

    #[tokio::test]
    async fn a_missing_content_type_is_a_415_that_says_what_to_send() {
        let (status, body) = post_body(r#"{"amount": 1}"#, None).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("application/json"));
    }

    #[test]
    fn echoed_detail_is_bounded_and_stripped_of_control_characters() {
        let hostile = format!("{}\n\r\0{}", "a".repeat(MAX_ECHO), "b".repeat(50));
        let out = sanitize(&hostile);
        assert!(out.chars().count() <= MAX_ECHO + 1, "got {} chars", out.chars().count());
        assert!(!out.contains('\n') && !out.contains('\r') && !out.contains('\0'));
    }

    #[test]
    fn param_extraction_only_fires_on_something_path_shaped() {
        assert_eq!(
            serde_param("card.exp_month: invalid type").as_deref(),
            Some("card.exp_month")
        );
        assert_eq!(serde_param("notes[0]: too long").as_deref(), Some("notes[0]"));
        // Prose, not a path — must not become a `param`.
        assert_eq!(serde_param("expected value at line 1 column 2"), None);
        assert_eq!(serde_param(""), None);
    }
}
