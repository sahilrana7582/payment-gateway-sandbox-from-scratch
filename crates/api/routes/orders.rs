//! Orders: create, retrieve, list.
//!
//! Request DTOs live here beside the handlers that consume them. Response
//! bodies do not: they come from `engine::event::order_json`, the SAME builder
//! that shapes webhook payloads. API responses and webhooks therefore cannot
//! drift apart, because there is exactly one definition of the wire shape. A
//! separate `OrderResponse` struct here would be a second definition, and the
//! two would diverge the first time a field was added in a hurry.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use domain::api_key::Scope;
use domain::id::OrderId;
use domain::money::Currency;
use engine::CreateOrderInput;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::extract::{Json, Path, Query};
use crate::pagination::ListParams;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/orders", post(create).get(list))
        .route("/orders/{id}", get(retrieve))
}

/// `deny_unknown_fields` on every request DTO in this crate is a deliberate
/// stance: a merchant who sends `{"amout": 500}` should get a 400 naming the
/// typo, not a 201 for an order with the default amount. Silent acceptance of
/// unknown parameters is how integration bugs reach production.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOrderRequest {
    pub amount: i64,
    pub currency: String,
    #[serde(default)]
    pub receipt: Option<String>,
    #[serde(default)]
    pub notes: HashMap<String, String>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreateOrderRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    auth.require(Scope::OrdersWrite)?;

    // Currency is the one field the engine cannot check for us: it takes a
    // parsed `Currency`, so the string-to-enum step is necessarily the edge's.
    let currency: Currency = req.currency.parse().map_err(|_| {
        crate::error::ApiError::invalid_request(
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

async fn retrieve(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::OrdersRead)?;

    // A malformed id reads as "not found", never as "malformed". Answering
    // differently would let a caller probe which ids exist by watching the
    // status change — and the merchant's next action is the same either way.
    let id: OrderId = id
        .parse()
        .map_err(|_| crate::error::ApiError::not_found("order"))?;

    let order = state.orders.get(auth.merchant_id, id).await?;
    Ok(Json(engine::event::order_json(&order)))
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Query(params): Query<ListParams>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::OrdersRead)?;

    let page = params.resolve(&state.config)?;
    let orders = state
        .orders
        .list(auth.merchant_id, page.before, page.fetch_limit())
        .await?;

    Ok(Json(
        page.finish(orders, engine::event::order_json, |o| o.created_at)
            .into_json(),
    ))
}
