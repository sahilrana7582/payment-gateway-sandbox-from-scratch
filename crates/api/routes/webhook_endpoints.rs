//! Webhook endpoints: register, list, disable.
//!
//! The signing secret is the only value in this API that is shown once and then
//! never again. That is a deliberate asymmetry with the rest of the resources:
//! a merchant who loses it registers a new endpoint, which is cheap, whereas an
//! endpoint whose secret can be re-read turns any read-scoped leak into the
//! ability to forge deliveries. [`render`] is the single serialiser used by
//! every path except creation, so the secret cannot escape by someone adding a
//! new list-shaped route later.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::Router;
use domain::api_key::Scope;
use domain::id::WebhookEndpointId;
use serde::Deserialize;
use serde_json::{json, Value};
use store::webhook_endpoint::WebhookEndpoint;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Path};
use crate::state::AppState;
use crate::validate;

/// How many active endpoints one merchant may hold.
///
/// Every event fans out to every active endpoint, so this number multiplies the
/// delivery worker's queue depth. It is a blast-radius control, not a plan
/// limit: a merchant looping a registration call should hit a clear 400 long
/// before their own events start starving other merchants' deliveries.
const MAX_ACTIVE_ENDPOINTS: usize = 16;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/webhook_endpoints", post(create).get(list))
        .route("/webhook_endpoints/{id}", delete(disable))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhookEndpointRequest {
    pub url: String,
    /// Empty means "every event type" — the same convention
    /// [`WebhookEndpoint::wants`] applies at delivery time.
    #[serde(default)]
    pub enabled_events: Vec<String>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreateWebhookEndpointRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    auth.require(Scope::WebhooksManage)?;

    let url = validate::webhook_url(&req.url)?;
    validate::enabled_events(&req.enabled_events)?;

    // Checked before minting a secret so a rejected request leaves no trace.
    // This is a read-then-write race under concurrency; the consequence of
    // losing it is one endpoint over the cap, which is harmless. A unique
    // constraint would be the wrong tool for a count.
    let existing = store::webhook_endpoint::list_active_for_merchant(&state.pool, auth.merchant_id)
        .await?
        .len();
    if existing >= MAX_ACTIVE_ENDPOINTS {
        return Err(ApiError::invalid_request(
            "endpoint_limit_reached",
            format!(
                "At most {MAX_ACTIVE_ENDPOINTS} active webhook endpoints are allowed. \
                 Delete one before adding another."
            ),
            None,
        ));
    }

    let endpoint = WebhookEndpoint {
        id: WebhookEndpointId::new(),
        merchant_id: auth.merchant_id,
        url,
        signing_secret: crypto::signing::generate_webhook_secret(),
        enabled_events: req.enabled_events,
        disabled_at: None,
        created_at: state.clock.now(),
    };
    store::webhook_endpoint::insert(&state.pool, &endpoint).await?;

    // The one response that carries the secret. Note it is built by hand rather
    // than by `render`, so revealing it stays an explicit act at exactly one
    // call site.
    let mut body = render(&endpoint);
    body["signing_secret"] = json!(endpoint.signing_secret);

    Ok((StatusCode::CREATED, Json(body)))
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::WebhooksManage)?;

    // Not paginated, and deliberately so: `MAX_ACTIVE_ENDPOINTS` already bounds
    // the result set, so a cursor here would be ceremony around a list that
    // cannot exceed sixteen rows. The `object: "list"` shape still matches the
    // paginated collections so a client can treat them uniformly.
    let endpoints =
        store::webhook_endpoint::list_active_for_merchant(&state.pool, auth.merchant_id).await?;

    Ok(Json(json!({
        "object": "list",
        "data": endpoints.iter().map(render).collect::<Vec<_>>(),
        "has_more": false,
    })))
}

/// Disabling rather than deleting: the outbox may still hold undelivered jobs
/// for this endpoint, and their rows reference it. `disabled_at` stops new
/// deliveries while keeping the delivery history readable.
async fn disable(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::WebhooksManage)?;

    let id: WebhookEndpointId = id
        .parse()
        .map_err(|_| ApiError::not_found("webhook endpoint"))?;

    let endpoint = store::webhook_endpoint::find_by_id(&state.pool, id).await?;
    // Ownership failures read as "not found", never as "forbidden" — the same
    // rule every other resource follows, so endpoint ids cannot be probed.
    if endpoint.merchant_id != auth.merchant_id {
        return Err(ApiError::not_found("webhook endpoint"));
    }

    // Idempotent: disabling an already-disabled endpoint is a no-op success, so
    // a retried DELETE does not surprise the caller with a 404.
    if endpoint.is_active() {
        store::webhook_endpoint::set_disabled(&state.pool, id, Some(state.clock.now())).await?;
    }

    Ok(Json(json!({
        "id": endpoint.id.to_string(),
        "object": "webhook_endpoint",
        "deleted": true,
    })))
}

/// The safe projection of an endpoint. Every path but creation goes through it.
fn render(e: &WebhookEndpoint) -> Value {
    json!({
        "id": e.id.to_string(),
        "object": "webhook_endpoint",
        "url": e.url,
        "enabled_events": e.enabled_events,
        "created_at": crate::routes::rfc3339(e.created_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::id::MerchantId;
    use time::macros::datetime;

    fn endpoint() -> WebhookEndpoint {
        WebhookEndpoint {
            id: WebhookEndpointId::new(),
            merchant_id: MerchantId::new(),
            url: "https://shop.acme.dev/hooks".into(),
            signing_secret: "whsec_do_not_leak_me".into(),
            enabled_events: vec!["payment.captured".into()],
            disabled_at: None,
            created_at: datetime!(2026-01-01 00:00:00 UTC),
        }
    }

    #[test]
    fn the_shared_projection_never_carries_the_signing_secret() {
        let rendered = render(&endpoint());
        assert!(rendered["signing_secret"].is_null());
        assert!(!rendered.to_string().contains("do_not_leak_me"));
    }

    #[test]
    fn the_projection_carries_what_a_merchant_needs_to_identify_an_endpoint() {
        let e = endpoint();
        let rendered = render(&e);
        assert_eq!(rendered["id"], e.id.to_string());
        assert_eq!(rendered["object"], "webhook_endpoint");
        assert_eq!(rendered["url"], e.url);
        assert_eq!(rendered["enabled_events"][0], "payment.captured");
        assert_eq!(rendered["created_at"], "2026-01-01T00:00:00Z");
    }
}
