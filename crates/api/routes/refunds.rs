//! Refunds — returning captured money to the cardholder.
//!
//! # Why `amount` is optional
//!
//! Omitting it refunds the entire remaining balance. That is not just
//! convenience: a merchant computing the remainder client-side has to read the
//! payment, subtract prior refunds, and POST — and another refund can land
//! between the read and the POST, turning their "full refund" into an
//! over-refund 400. Letting the engine resolve the remainder under the payment
//! row lock removes the race entirely. Callers who genuinely want a partial
//! refund still pass an explicit amount.
//!
//! # Why over-refunding is a 400 and not a 409
//!
//! A 409 says "retry and it may work". Over-refunding never becomes valid by
//! retrying — the refundable balance only shrinks. It is a bad parameter, so it
//! is a 400 naming `amount`, and the message states the balance so the caller
//! can correct it in one round trip rather than guessing.
//!
//! # Why listing requires a payment id
//!
//! There is no "all refunds for a merchant" query in the store, and adding one
//! would need its own cursor index. A refund is only ever interesting relative
//! to its payment, so the collection is scoped: `?payment_id=pay_...`. The
//! ownership check runs on the *payment*, which means an id belonging to
//! another merchant is a 404 rather than an empty list — an empty list would
//! confirm the id exists.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Router};
use domain::api_key::Scope;
use domain::id::{PaymentId, RefundId};
use engine::refund::CreateRefundInput;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Path, Query};
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/refunds", post(create).get(list))
        .route("/refunds/{id}", get(retrieve))
}

/// Refund creation.
///
/// `amount` is in minor units, consistent with every other amount on the API.
/// Omit it to refund the full remaining balance.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRefundRequest {
    payment_id: String,
    #[serde(default)]
    amount: Option<i64>,
    #[serde(default)]
    reason: Option<String>,
}

/// Collection filter. `payment_id` is required — see the module docs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListRefundsParams {
    payment_id: String,
}

async fn create(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreateRefundRequest>,
) -> ApiResult<impl IntoResponse> {
    auth.require(Scope::RefundsWrite)?;

    // An unparseable id is "no such payment", never a 400 — ids must not be
    // probeable by shape.
    let payment_id: PaymentId = req
        .payment_id
        .parse()
        .map_err(|_| ApiError::not_found("payment"))?;

    let refund = state
        .refunds
        .create(
            auth.merchant_id,
            CreateRefundInput {
                payment_id,
                amount_minor: req.amount,
                reason: req.reason,
            },
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(engine::event::refund_json(&refund)),
    ))
}

async fn retrieve(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<impl IntoResponse> {
    // A refund is payment data, and there is no `refunds:read` scope; reading
    // one is gated on `payments:read` so a read-only key works here too.
    auth.require(Scope::PaymentsRead)?;

    let id: RefundId = id.parse().map_err(|_| ApiError::not_found("refund"))?;
    let refund = state.refunds.get(auth.merchant_id, id).await?;
    Ok(axum::Json(engine::event::refund_json(&refund)))
}

async fn list(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Query(params): Query<ListRefundsParams>,
) -> ApiResult<impl IntoResponse> {
    auth.require(Scope::PaymentsRead)?;

    let payment_id: PaymentId = params
        .payment_id
        .parse()
        .map_err(|_| ApiError::not_found("payment"))?;

    let refunds = state
        .refunds
        .list_for_payment(auth.merchant_id, payment_id)
        .await?;

    // Deliberately not paginated. Refunds against a single payment are bounded
    // in practice by the payment amount divided by the smallest refund, and
    // every real one has a handful at most. If that assumption ever breaks,
    // this becomes a cursor collection like the others.
    Ok(axum::Json(json!({
        "object": "list",
        "data": refunds.iter().map(engine::event::refund_json).collect::<Vec<Value>>(),
        "has_more": false,
        "next_before": Value::Null,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_field_is_rejected_rather_than_silently_ignored() {
        // `deny_unknown_fields` is what turns a typo like "ammount" into a 400
        // instead of an accidental full refund.
        let raw = r#"{"payment_id":"pay_1","ammount":500}"#;
        assert!(serde_json::from_str::<CreateRefundRequest>(raw).is_err());
    }

    #[test]
    fn amount_and_reason_are_optional() {
        let raw = r#"{"payment_id":"pay_1"}"#;
        let req: CreateRefundRequest =
            serde_json::from_str(raw).expect("payment_id alone must be a valid body");
        assert!(req.amount.is_none());
        assert!(req.reason.is_none());
    }

    #[test]
    fn listing_without_a_payment_id_is_rejected() {
        assert!(serde_json::from_str::<ListRefundsParams>(r#"{}"#).is_err());
    }
}
