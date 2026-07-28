//! Request correlation.
//!
//! Every request gets an id. It goes into the tracing span, onto the response as
//! `x-request-id`, and into the body of any error envelope we produce. That last
//! part is the point of the whole file: when a merchant opens a support ticket
//! saying "my payment failed", the only useful first question is "what was the
//! request id", and they can only answer it if we put it somewhere they can copy
//! out of their own logs.
//!
//! # Trusting the caller's id
//!
//! An inbound `x-request-id` is honoured, so a trace started at the merchant's
//! edge proxy survives into ours. It is validated first ([`is_acceptable`]) —
//! this value ends up in log lines and in a response header, and an unvalidated
//! header echoed into both is a log-injection and header-splitting primitive.
//! Anything we do not like is silently replaced rather than rejected: a bad
//! correlation id is not worth failing a payment over.
//!
//! # Why a task-local
//!
//! [`ApiError::into_response`](crate::error::ApiError) has no access to the
//! request. Threading an id through every handler signature to reach it would be
//! noise on hundreds of call sites for a field nine tenths of them never touch.
//! The id is instead published as a task-local for the duration of the request,
//! and read at the one place that needs it. It is scoped to a single request's
//! future, so there is no ambient-state hazard: a task that was not started by
//! this middleware simply sees `None`.

use axum::extract::Request;
use axum::http::header::HeaderName;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{field, info_span, Instrument};
use uuid::Uuid;

/// The header we read and write. Lowercase because HTTP/2 requires it and
/// `HeaderName` comparison is case-insensitive anyway.
pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Bounds on an id we are willing to adopt from the caller. Long enough for a
/// UUID with separators or a trace parent; short enough that a header full of
/// junk cannot bloat every log line for the request.
const MIN_LEN: usize = 8;
const MAX_LEN: usize = 128;

tokio::task_local! {
    static CURRENT: RequestId;
}

/// A validated request id. Cheap to clone; also placed in request extensions so
/// handlers that want it can take it as `Extension<RequestId>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(String);

impl RequestId {
    /// Mint a fresh id. v7 UUIDs sort by creation time, so ids from one incident
    /// group together in a log search.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("req_{}", Uuid::now_v7().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The id of the request being served on this task, if any.
#[must_use]
pub fn current() -> Option<RequestId> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Whether an inbound id is safe to adopt.
///
/// Only unreserved URL characters are allowed. That excludes CR, LF and NUL
/// (log injection, header splitting), excludes whitespace and quotes (which
/// break every log parser that splits on them), and excludes non-ASCII entirely
/// so the value round-trips through `HeaderValue` without surprises.
fn is_acceptable(candidate: &str) -> bool {
    (MIN_LEN..=MAX_LEN).contains(&candidate.len())
        && candidate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Adopt or mint a request id, publish it, and echo it on the response.
///
/// Runs outside the trace layer so that every log line for the request —
/// including ones written by layers below — carries the id.
pub async fn propagate(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(&HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| is_acceptable(v))
        .map(|v| RequestId(v.to_owned()))
        .unwrap_or_else(RequestId::generate);

    // `is_acceptable` guarantees this is header-safe, so the fallback is
    // unreachable; it exists so a future relaxation of the charset cannot turn
    // into a panic.
    let header_value =
        HeaderValue::from_str(id.as_str()).unwrap_or_else(|_| HeaderValue::from_static("req_x"));

    req.headers_mut().insert(HEADER, header_value.clone());
    req.extensions_mut().insert(id.clone());

    let span = info_span!(
        "http.request",
        request_id = %id,
        method = %req.method(),
        // Recorded by the trace layer once routing has matched; declared here so
        // the field exists on the span from the first log line onwards.
        status = field::Empty,
    );

    let mut response = CURRENT.scope(id, next.run(req).instrument(span)).await;

    // A handler that set its own correlation header keeps it.
    response.headers_mut().entry(HEADER).or_insert(header_value);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route(
                "/",
                get(|| async {
                    // Proves the task-local is visible from inside the handler,
                    // which is what the error envelope depends on.
                    current().map(|id| id.to_string()).unwrap_or_default()
                }),
            )
            .layer(axum::middleware::from_fn(propagate))
    }

    async fn call(header: Option<&str>) -> (StatusCode, String, String) {
        let mut builder = axum::http::Request::builder().uri("/");
        if let Some(h) = header {
            builder = builder.header(HEADER, h);
        }
        let res = app()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();

        let status = res.status();
        let echoed = res
            .headers()
            .get(HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = String::from_utf8(
            axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        (status, echoed, body)
    }

    #[test]
    fn generated_ids_are_prefixed_and_unique() {
        let a = RequestId::generate();
        let b = RequestId::generate();
        assert!(a.as_str().starts_with("req_"));
        assert_ne!(a, b);
        assert!(is_acceptable(a.as_str()));
    }

    #[test]
    fn acceptable_ids_are_bounded_and_log_safe() {
        assert!(is_acceptable("0123456789abcdef"));
        assert!(is_acceptable("trace-01:02.03_ab"));

        assert!(!is_acceptable("short"));
        assert!(!is_acceptable(&"a".repeat(MAX_LEN + 1)));
        assert!(!is_acceptable("has spaces here"));
        assert!(!is_acceptable("inject\nlevel=ERROR"));
        assert!(!is_acceptable("quote\"break"));
        assert!(!is_acceptable("emoji-🙂-here"));
    }

    #[tokio::test]
    async fn an_id_is_minted_when_none_is_offered() {
        let (status, echoed, body) = call(None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(echoed.starts_with("req_"));
        assert_eq!(body, echoed, "handler must observe the same id we echo");
    }

    #[tokio::test]
    async fn a_well_formed_caller_id_is_adopted() {
        let (_, echoed, body) = call(Some("edge-7f3c9a21-0004")).await;
        assert_eq!(echoed, "edge-7f3c9a21-0004");
        assert_eq!(body, "edge-7f3c9a21-0004");
    }

    #[tokio::test]
    async fn a_hostile_caller_id_is_replaced_not_echoed() {
        let (status, echoed, _) = call(Some("aaaaaaaa\tinjected")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(echoed.starts_with("req_"), "got {echoed}");
    }
}
