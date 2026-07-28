//! Health probes. Three of them, because "is it healthy" is three questions.
//!
//! * `/health/live` — is the process running? Never touches a dependency. A
//!   liveness probe that fails on a database blip gets the container killed and
//!   restarted, which does nothing for the database and drops in-flight
//!   payments. This one can only fail by not answering.
//! * `/health/ready` — should this instance receive traffic? Checks the pool,
//!   so an instance that cannot reach Postgres is pulled from the load balancer
//!   and put back when it recovers, without a restart.
//! * `/health` — the human/uptime-monitor entry point. Same depth as `ready`,
//!   and the shape existing callers already expect.
//!
//! All three are unauthenticated, which is why none of them report *why* the
//! database is unreachable: an error string here is free reconnaissance about
//! internal hostnames and versions.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header::{HeaderValue, CACHE_CONTROL};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(deep))
        .route("/health/live", get(live))
        .route("/health/ready", get(deep))
}

/// Process liveness. Deliberately trivial — see the module docs.
async fn live() -> impl IntoResponse {
    no_store((StatusCode::OK, Json(json!({ "status": "ok" }))))
}

/// Dependency readiness.
async fn deep(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // A health check that does not touch the database reports a zombie as
    // healthy. One trivial query proves the whole path: pool, connection,
    // credentials, and the server actually answering.
    let body = match sqlx::query_scalar!("SELECT 1 AS one")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(err) => {
            // Logged in full here, summarised to one word on the wire.
            tracing::warn!(error = %err, "health check could not reach the database");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "database_unreachable" })),
            )
        }
    };
    no_store(body)
}

/// Probe responses must never be cached: a cached "ok" outlives the outage it
/// was supposed to report, and intermediaries do cache 200s by default.
fn no_store(response: impl IntoResponse) -> axum::response::Response {
    let mut response = response.into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn liveness_answers_without_a_database_and_forbids_caching() {
        // No pool is ever connected here — that is the point of the probe.
        let app: Router = Router::new().route("/health/live", get(live));

        let response = app
            .oneshot(Request::builder().uri("/health/live").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }
}
