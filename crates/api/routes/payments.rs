//! Payments: create, retrieve, capture.
//!
//! This is the only route module that handles cardholder data, and that shapes
//! everything about it. Three rules apply here and nowhere else:
//!
//! 1. **The PAN and CVC never reach a log line.** [`CardRequest`] has a
//!    hand-written `Debug` that redacts both, so `?req` in a future `tracing`
//!    call — or a panic message, or a `dbg!` left in during an incident — cannot
//!    leak a card number. Deriving `Debug` here would put the PAN one careless
//!    interpolation away from disk.
//! 2. **The CVC is never stored.** It is shape-checked and dropped. Nothing in
//!    `engine` or `store` even has a field for it.
//! 3. **Unknown cards never touch the database.** The simulator gate below runs
//!    before any write, so a real card number produces no payment row, no
//!    attempt row and no log entry — just a rejection.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Router;
use domain::api_key::Scope;
use domain::id::{OrderId, PaymentId};
use domain::payment_status::PaymentStatus;
use engine::payment::{AttemptDecision, AttemptOutcome};
use engine::CreatePaymentInput;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Path};
use crate::state::AppState;
use crate::validate;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/payments", post(create))
        .route("/payments/{id}", get(retrieve))
        .route("/payments/{id}/capture", post(capture))
}

// ---------------------------------------------------------------------------
// Request DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardRequest {
    pub number: String,
    pub exp_month: u8,
    pub exp_year: u16,
    /// Accepted, shape-checked, and immediately discarded — a CVC is never
    /// stored anywhere, matching PCI rules.
    #[serde(default)]
    pub cvc: Option<String>,
}

/// Redacting `Debug`. See the module docs: this is a security control, not a
/// formatting preference, and it is the reason `Debug` is not derived.
impl fmt::Debug for CardRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CardRequest")
            // Not even the last four: this type exists before validation, so
            // the field may hold anything a client sent.
            .field("number", &"[redacted]")
            .field("exp_month", &self.exp_month)
            .field("exp_year", &self.exp_year)
            .field("cvc", &self.cvc.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePaymentRequest {
    pub order_id: String,
    pub card: CardRequest,
    #[serde(default)]
    pub notes: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn create(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Json(req): Json<CreatePaymentRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    auth.require(Scope::PaymentsWrite)?;

    if let Some(cvc) = req.card.cvc.as_deref() {
        validate::cvc(cvc)?;
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

    if state.config.simulate_latency {
        // Bounded by configuration, not by trust in the simulator: an artificial
        // delay must never be able to outlive the request budget.
        let delay = std::time::Duration::from_millis(u64::from(decision.latency_ms))
            .min(state.config.max_simulated_latency);
        tokio::time::sleep(delay).await;
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

async fn retrieve(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::PaymentsRead)?;
    let id: PaymentId = id.parse().map_err(|_| ApiError::not_found("payment"))?;
    let payment = state.payments.get(auth.merchant_id, id).await?;
    Ok(Json(engine::event::payment_json(&payment)))
}

async fn capture(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    auth.require(Scope::PaymentsWrite)?;
    let id: PaymentId = id.parse().map_err(|_| ApiError::not_found("payment"))?;
    let payment = state.payments.capture(auth.merchant_id, id).await?;
    Ok(Json(engine::event::payment_json(&payment)))
}

// ---------------------------------------------------------------------------
// Vocabulary translation
// ---------------------------------------------------------------------------

/// simulator vocabulary → engine vocabulary. The one place they meet.
///
/// `engine` does not depend on `simulator` by design, so this translation has to
/// happen somewhere; putting it here keeps the crate graph acyclic and keeps
/// every engine outcome testable without a simulator in sight.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> CardRequest {
        CardRequest {
            number: "4242424242424242".into(),
            exp_month: 12,
            exp_year: 2030,
            cvc: Some("123".into()),
        }
    }

    #[test]
    fn debug_output_never_contains_the_pan_or_the_cvc() {
        let rendered = format!("{:?}", card());
        assert!(!rendered.contains("4242424242424242"), "{rendered}");
        assert!(!rendered.contains("123"), "{rendered}");
        assert!(rendered.contains("[redacted]"));
        // Expiry is not cardholder data and stays visible for debugging.
        assert!(rendered.contains("2030"));
    }

    #[test]
    fn the_enclosing_request_debug_is_safe_too() {
        let req = CreatePaymentRequest {
            order_id: "order_1".into(),
            card: card(),
            notes: HashMap::new(),
        };
        assert!(!format!("{req:?}").contains("4242424242424242"));
    }

    #[test]
    fn an_absent_cvc_renders_as_absent_rather_than_redacted() {
        let rendered = format!(
            "{:?}",
            CardRequest {
                cvc: None,
                ..card()
            }
        );
        assert!(rendered.contains("cvc: None"), "{rendered}");
    }

    #[test]
    fn every_simulator_outcome_maps_to_an_engine_outcome() {
        use simulator::Outcome;

        let cases = [
            (Outcome::Success, AttemptOutcome::Capture),
            (
                Outcome::SuccessThenDispute,
                AttemptOutcome::CaptureThenDispute,
            ),
            (Outcome::Authorize, AttemptOutcome::Authorize),
            (Outcome::RequiresAction, AttemptOutcome::RequireAction),
            (Outcome::RiskHold, AttemptOutcome::RiskHold),
        ];

        for (from, expected) in cases {
            let mapped = map_decision(simulator::Decision {
                outcome: from,
                latency_ms: 42,
            });
            assert_eq!(mapped.outcome, expected);
            assert_eq!(mapped.latency_ms, 42);
        }
    }

    #[test]
    fn a_decline_carries_its_code_and_description_across() {
        let mapped = map_decision(simulator::Decision {
            outcome: simulator::Outcome::Declined(simulator::DeclineCode::InsufficientFunds),
            latency_ms: 10,
        });
        match mapped.outcome {
            AttemptOutcome::Decline { code, description } => {
                assert_eq!(code, "insufficient_funds");
                assert!(!description.is_empty());
            }
            other => panic!("expected a decline, got {other:?}"),
        }
    }
}
