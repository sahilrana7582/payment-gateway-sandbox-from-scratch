//! Request handlers. DTOs live beside their handlers; carve out a /dto module
//! when the count justifies it.
//!
//! Responses reuse `engine::event::{order_json, payment_json}` — the SAME
//! builders that shape webhook payloads. API responses and webhooks can never
//! drift apart, because there is exactly one definition of each wire shape.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use domain::api_key::Scope;
use domain::id::{OrderId, PaymentId, WebhookEndpointId};
use domain::money::Currency;
use domain::payment_status::PaymentStatus;
use engine::payment::{AttemptDecision, AttemptOutcome};
use engine::{CreateOrderInput, CreatePaymentInput};
use serde::Deserialize;
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::auth::AuthCtx;
use crate::error::ApiError;
use crate::state::AppState;

// ============================================================================
// Orders
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateOrderRequest {
    pub amount: i64,
    pub currency: String,
    pub receipt: Option<String>,
    #[serde(default)]
    pub notes: HashMap<String, String>,
}

pub async fn create_order(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::OrdersWrite)?;

    let currency: Currency = req.currency.parse().map_err(|_| {
        ApiError::invalid_request(
            "currency_invalid",
            format!("'{}' is not a supported currency.", req.currency),
            Some("currency"),
        )
    })?;

    let order = state
        .orders
        .create(
            auth.merchant_id,
            CreateOrderInput {
                amount_minor: req.amount,
                currency,
                receipt: req.receipt,
                notes: req.notes,
            },
        )
        .await?;

    Ok((StatusCode::CREATED, Json(engine::event::order_json(&order))))
}

pub async fn get_order(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::OrdersRead)?;
    let id: OrderId = id.parse().map_err(|_| ApiError::not_found("order"))?;
    let order = state.orders.get(auth.merchant_id, id).await?;
    Ok(Json(engine::event::order_json(&order)))
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<i64>,
    /// RFC3339 timestamp cursor: return items created strictly before this.
    pub before: Option<String>,
}

pub async fn list_orders(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::OrdersRead)?;

    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let before = parse_before(params.before.as_deref())?;

    // Fetch one extra to compute has_more without a count query.
    let mut orders = state
        .orders
        .list(auth.merchant_id, before, limit + 1)
        .await?;
    // `limit` is clamped to 1..=50 above, so the conversion always succeeds.
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let has_more = orders.len() > limit;
    orders.truncate(limit);

    Ok(Json(json!({
        "data": orders.iter().map(engine::event::order_json).collect::<Vec<_>>(),
        "has_more": has_more,
    })))
}

fn parse_before(raw: Option<&str>) -> Result<Option<OffsetDateTime>, ApiError> {
    match raw {
        None => Ok(None),
        Some(s) => OffsetDateTime::parse(s, &Rfc3339).map(Some).map_err(|_| {
            ApiError::invalid_request(
                "cursor_invalid",
                "'before' must be an RFC3339 timestamp.",
                Some("before"),
            )
        }),
    }
}

// ============================================================================
// Payments
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CardRequest {
    pub number: String,
    pub exp_month: u8,
    pub exp_year: u16,
    /// Accepted, shape-checked, and immediately discarded — a CVC is never
    /// stored anywhere, matching PCI rules.
    pub cvc: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePaymentRequest {
    pub order_id: String,
    pub card: CardRequest,
    #[serde(default)]
    pub notes: HashMap<String, String>,
}

pub async fn create_payment(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreatePaymentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::PaymentsWrite)?;

    if let Some(cvc) = &req.cvc_ref() {
        if !(3..=4).contains(&cvc.len()) || !cvc.chars().all(|c| c.is_ascii_digit()) {
            return Err(ApiError::card(
                "incorrect_cvc",
                "The security code is malformed.",
                None,
            ));
        }
    }

    let order_id: OrderId = req
        .order_id
        .parse()
        .map_err(|_| ApiError::not_found("order"))?;

    // The order supplies the amount the simulator needs. Ownership is checked
    // here; the engine re-locks and re-checks inside the transaction.
    let order = state.orders.get(auth.merchant_id, order_id).await?;

    // The simulator gate: an unknown card gets no payment row, no log entry,
    // nothing — this is the control that keeps real cards out of the system.
    let decision = simulator::decide(&req.card.number, order.amount).map_err(|_| {
        ApiError::card(
            "card_not_in_test_set",
            "This card number is not in the published test set. Only documented test cards are accepted — see /docs/testing/test-cards.",
            None,
        )
    })?;

    if state.simulate_latency {
        tokio::time::sleep(std::time::Duration::from_millis(decision.latency_ms as u64)).await;
    }

    let payment = state
        .payments
        .create(
            auth.merchant_id,
            CreatePaymentInput {
                order_id,
                card_number: req.card.number,
                card_exp_month: req.card.exp_month,
                card_exp_year: req.card.exp_year,
                notes: req.notes,
            },
            map_decision(decision),
        )
        .await?;

    // A decline commits the failed payment, THEN reports the card error with
    // the payment id attached — the merchant can always fetch what happened.
    if payment.status == PaymentStatus::Failed {
        return Err(ApiError::card(
            payment.error_code.as_deref().unwrap_or("card_declined"),
            payment
                .error_description
                .clone()
                .unwrap_or_else(|| "The card was declined.".into()),
            Some(payment.id.to_string()),
        ));
    }

    Ok((
        StatusCode::CREATED,
        Json(engine::event::payment_json(&payment)),
    ))
}

impl CreatePaymentRequest {
    fn cvc_ref(&self) -> Option<&String> {
        self.card.cvc.as_ref()
    }
}

/// simulator vocabulary → engine vocabulary. The one place they meet.
fn map_decision(d: simulator::Decision) -> AttemptDecision {
    let outcome = match d.outcome {
        simulator::Outcome::Success => AttemptOutcome::Capture,
        simulator::Outcome::SuccessThenDispute => AttemptOutcome::CaptureThenDispute,
        simulator::Outcome::Authorize => AttemptOutcome::Authorize,
        simulator::Outcome::Declined(code) => AttemptOutcome::Decline {
            code: code.as_str().to_string(),
            description: code.description().to_string(),
        },
        simulator::Outcome::RequiresAction => AttemptOutcome::RequireAction,
        simulator::Outcome::RiskHold => AttemptOutcome::RiskHold,
    };
    AttemptDecision {
        outcome,
        latency_ms: d.latency_ms,
    }
}

pub async fn get_payment(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::PaymentsRead)?;
    let id: PaymentId = id.parse().map_err(|_| ApiError::not_found("payment"))?;
    let payment = state.payments.get(auth.merchant_id, id).await?;
    Ok(Json(engine::event::payment_json(&payment)))
}

pub async fn capture_payment(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::PaymentsWrite)?;
    let id: PaymentId = id.parse().map_err(|_| ApiError::not_found("payment"))?;
    let payment = state.payments.capture(auth.merchant_id, id).await?;
    Ok(Json(engine::event::payment_json(&payment)))
}

// ============================================================================
// Webhook endpoints
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateWebhookEndpointRequest {
    pub url: String,
    #[serde(default)]
    pub enabled_events: Vec<String>,
}

pub async fn create_webhook_endpoint(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreateWebhookEndpointRequest>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::WebhooksManage)?;

    if !(req.url.starts_with("https://") || req.url.starts_with("http://")) {
        return Err(ApiError::invalid_request(
            "url_invalid",
            "Webhook URL must start with http:// or https://.",
            Some("url"),
        ));
    }

    let endpoint = store::webhook_endpoint::WebhookEndpoint {
        id: WebhookEndpointId::new(),
        merchant_id: auth.merchant_id,
        url: req.url,
        signing_secret: crypto::signing::generate_webhook_secret(),
        enabled_events: req.enabled_events,
        disabled_at: None,
        created_at: state.clock.now(),
    };
    store::webhook_endpoint::insert(&state.pool, &endpoint)
        .await
        .map_err(|_| ApiError::internal())?;

    // The signing secret appears in THIS response and never again.
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": endpoint.id.to_string(),
            "url": endpoint.url,
            "signing_secret": endpoint.signing_secret,
            "enabled_events": endpoint.enabled_events,
        })),
    ))
}

pub async fn list_webhook_endpoints(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::WebhooksManage)?;
    let endpoints =
        store::webhook_endpoint::list_active_for_merchant(&state.pool, auth.merchant_id)
            .await
            .map_err(|_| ApiError::internal())?;

    // No signing_secret here — shown once at creation, then gone.
    Ok(Json(json!({
        "data": endpoints.iter().map(|e| json!({
            "id": e.id.to_string(),
            "url": e.url,
            "enabled_events": e.enabled_events,
        })).collect::<Vec<_>>(),
    })))
}

// ============================================================================
// Events (debugging aid for integrators — "what did the gateway see?")
// ============================================================================

pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Query(params): Query<ListParams>,
) -> Result<impl IntoResponse, ApiError> {
    auth.require(Scope::PaymentsRead)?;

    let limit = params.limit.unwrap_or(10).clamp(1, 50);
    let before = parse_before(params.before.as_deref())?;
    let events = store::event::list_for_merchant(&state.pool, auth.merchant_id, before, limit)
        .await
        .map_err(|_| ApiError::internal())?;

    Ok(Json(json!({
        "data": events.iter().map(|e| json!({
            "id": e.id.to_string(),
            "type": e.event_type,
            "created_at": e.created_at.format(&Rfc3339).unwrap_or_default(),
            "api_version": e.api_version,
            "data": e.payload,
        })).collect::<Vec<_>>(),
    })))
}

// ============================================================================
// Health
// ============================================================================

pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // A health check that doesn't touch the database reports a zombie as
    // healthy. One trivial query proves the whole path.
    match sqlx::query_scalar!("SELECT 1 AS one")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "database_unreachable" })),
        ),
    }
}
