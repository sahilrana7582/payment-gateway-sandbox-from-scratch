//! Event emission — the transactional outbox in action.
//!
//! `emit` writes the event row, one pending delivery row per subscribed
//! endpoint, and one delivery job per delivery — ALL on the caller's
//! transaction. If the payment rolls back, every trace of the event vanishes
//! with it; if it commits, delivery is guaranteed to be attempted.
//!
//! The `events.payload` column stores only the `data` object. The delivery
//! worker assembles the full envelope `{id, type, created_at, api_version,
//! data}` at send time from the event row, so the envelope shape is defined
//! in exactly one place (the webhooks crate).

use domain::id::{EventId, MerchantId};
use domain::order::Order;
use domain::payment::Payment;
use queue::job::kinds;
use serde_json::{json, Value as Json};
use store::tx::PgTx;
use store::webhook_endpoint::WebhookEndpoint;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::EngineResult;

/// The payload shape version, sent on every event and webhook.
pub const API_VERSION: &str = "2026-07-01";

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

/// Emit an event and fan out deliveries to every active endpoint that
/// subscribes to this type. `endpoints` is pre-fetched by the caller (outside
/// the transaction — acceptable staleness; an endpoint registered mid-payment
/// catches the next event).
pub async fn emit(
    tx: &mut PgTx<'_>,
    endpoints: &[WebhookEndpoint],
    merchant_id: MerchantId,
    event_type: &str,
    data: Json,
    now: OffsetDateTime,
) -> EngineResult<EventId> {
    let event_id =
        store::event::insert(tx, merchant_id, event_type, &data, API_VERSION, now).await?;

    for ep in endpoints
        .iter()
        .filter(|e| e.is_active() && e.wants(event_type))
    {
        let delivery_id = store::webhook_delivery::insert_pending(tx, event_id, ep.id, now).await?;

        store::job::enqueue(
            tx,
            kinds::DELIVER_WEBHOOK,
            &json!({
                "delivery_id": delivery_id,
                "event_id": event_id.to_string(),
                "endpoint_id": ep.id.to_string(),
            }),
            now, // due immediately
            queue::scheduler::MAX_ATTEMPTS,
            now,
        )
        .await?;
    }

    Ok(event_id)
}

// ============================================================================
// Payload builders — the single source of truth for wire shapes.
// ============================================================================

pub fn payment_json(p: &Payment) -> Json {
    json!({
        "id": p.id.to_string(),
        "order_id": p.order_id.to_string(),
        "amount": p.amount.minor(),
        "currency": p.amount.currency().code(),
        "amount_refunded": p.amount_refunded,
        "status": p.status.to_string(),
        "method": p.method.as_str(),
        "card": p.card.as_ref().map(|c| json!({
            "last4": c.last4,
            "brand": c.brand.as_str(),
            "exp_month": c.exp_month,
            "exp_year": c.exp_year,
        })),
        "error_code": p.error_code,
        "error_description": p.error_description,
        "notes": p.notes,
        "created_at": rfc3339(p.created_at),
        "captured_at": p.captured_at.map(rfc3339),
    })
}

pub fn order_json(o: &Order) -> Json {
    json!({
        "id": o.id.to_string(),
        "amount": o.amount.minor(),
        "amount_paid": o.amount_paid,
        "currency": o.amount.currency().code(),
        "receipt": o.receipt,
        "status": o.status.to_string(),
        "notes": o.notes,
        "created_at": rfc3339(o.created_at),
    })
}

/// Payment events carry both objects, matching the webhook shape promised in
/// the docs: the merchant's handler finds their own order via `receipt`.
pub fn payment_event_data(p: &Payment, o: &Order) -> Json {
    json!({ "payment": payment_json(p), "order": order_json(o) })
}

pub fn order_event_data(o: &Order) -> Json {
    json!({ "order": order_json(o) })
}
