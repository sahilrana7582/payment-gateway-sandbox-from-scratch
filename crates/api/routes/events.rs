//! Events: the integrator's answer to "what did the gateway actually see?"
//!
//! Every event served here is the *same row* the webhook worker delivers from —
//! one transactional outbox, two readers. That is what makes this endpoint
//! useful during integration: when a webhook does not arrive, listing events
//! settles whether the gateway failed to emit it or the delivery failed, which
//! is otherwise the most expensive question in a payments integration.
//!
//! Pagination goes through [`crate::pagination`] like every other collection.
//! The previous implementation returned a bare `data` array with no `has_more`,
//! so a caller could not tell a last page from a full one.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::routing::get;
use axum::Router;
use domain::api_key::Scope;
use serde_json::{json, Value};
use store::event::Event;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::extract::{Json, Query};
use crate::pagination::ListParams;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/events", get(list))
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<Value>> {
    // Read scope, not manage: the event log is diagnostic, and gating it behind
    // a write-capable key would push integrators toward using secret keys where
    // a read key would do.
    auth.require(Scope::PaymentsRead)?;

    let page = params.resolve(&state.config)?;
    let events =
        store::event::list_for_merchant(&state.pool, auth.merchant_id, page.before, page.fetch_limit())
            .await?;

    Ok(Json(
        page.finish(events, render, |e| e.created_at).into_json(),
    ))
}

/// Wire shape of one event.
///
/// `data` holds the payload verbatim — byte-identical to what the webhook
/// delivery carried, which is the whole point of reading it here. `api_version`
/// travels with it so a merchant replaying an old event knows which schema it
/// was written against.
fn render(e: &Event) -> Value {
    json!({
        "id": e.id.to_string(),
        "object": "event",
        "type": e.event_type,
        "api_version": e.api_version,
        "created_at": crate::routes::rfc3339(e.created_at),
        "data": e.payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::id::{EventId, MerchantId};
    use time::macros::datetime;

    #[test]
    fn an_event_renders_with_its_payload_untouched() {
        let payload = json!({ "id": "pay_1", "status": "captured" });
        let event = Event {
            id: EventId::new(),
            merchant_id: MerchantId::new(),
            event_type: "payment.captured".into(),
            payload: payload.clone(),
            api_version: "2026-01-01".into(),
            created_at: datetime!(2026-01-01 00:00:00 UTC),
        };

        let rendered = render(&event);
        assert_eq!(rendered["object"], "event");
        assert_eq!(rendered["type"], "payment.captured");
        assert_eq!(rendered["api_version"], "2026-01-01");
        assert_eq!(rendered["created_at"], "2026-01-01T00:00:00Z");
        assert_eq!(rendered["data"], payload);
    }
}
