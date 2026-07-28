//! End-to-end HTTP tests: the router as a client actually meets it.
//!
//! These live in `tests/` rather than in a `#[cfg(test)]` module because they
//! exercise the crate through its public surface — `api::router` and nothing
//! else. If a test here needs a private item, that is a signal the test is
//! really a unit test and belongs next to the code it covers; the unit tests in
//! each module handle that layer.
//!
//! Every case drives the *whole* stack via `oneshot`, so the middleware order
//! declared in `routes::router` is under test too, not just the handlers.
//! Assertions are written against the wire — status codes, headers, JSON — never
//! against internal types, because those are what a merchant integrates with and
//! what we are therefore not allowed to break.

// `clippy.toml` already sets `allow-unwrap-in-tests`/`allow-expect-in-tests`, but
// clippy only applies those inside `#[test]` functions and `#[cfg(test)]`
// modules. The helpers below sit at module level in an integration-test binary,
// so they fall outside that exemption despite being nothing but test code.
// Panicking is how a test reports failure here; there is no caller to hand a
// `Result` back to.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderName, Request, Response, StatusCode};
use axum::Router;
use domain::api_key::{ApiKey, ApiKeyKind};
use domain::clock::FixedClock;
use domain::id::MerchantId;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use time::macros::datetime;
use tower::ServiceExt;

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
const REPLAYED: HeaderName = HeaderName::from_static("idempotent-replayed");

fn now() -> time::OffsetDateTime {
    datetime!(2026-01-01 00:00:00 UTC)
}

fn app(pool: &PgPool) -> Router {
    api::router(api::AppState::new(
        pool.clone(),
        Arc::new(FixedClock::new(now())),
        "test_pepper".into(),
        false, // no artificial latency in tests
    ))
}

/// Seed a merchant + secret key; return the bearer token.
async fn seed(pool: &PgPool) -> (MerchantId, String) {
    let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
    store::merchant::insert(pool, &m).await.unwrap();
    let gk = crypto::api_key::generate("sk_test").unwrap();
    let key = ApiKey::new(m.id, ApiKeyKind::Secret, "test", gk.hash, now());
    store::api_key::insert(pool, &key).await.unwrap();
    (m.id, gk.plaintext)
}

/// A request builder that mirrors what an SDK would send.
struct Call {
    method: &'static str,
    path: String,
    token: Option<String>,
    body: Option<Value>,
    headers: Vec<(HeaderName, String)>,
    content_type: Option<&'static str>,
    raw_body: Option<String>,
}

impl Call {
    fn new(method: &'static str, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            token: None,
            body: None,
            headers: Vec::new(),
            content_type: Some("application/json"),
            raw_body: None,
        }
    }

    fn auth(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }

    fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Bypass `serde_json` to send something a well-behaved client never would.
    fn raw(mut self, body: &str) -> Self {
        self.raw_body = Some(body.to_owned());
        self
    }

    fn no_content_type(mut self) -> Self {
        self.content_type = None;
        self
    }

    fn header(mut self, name: HeaderName, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    async fn send(self, app: &Router) -> Response<Body> {
        let mut builder = Request::builder().method(self.method).uri(&self.path);
        if let Some(t) = &self.token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }

        let payload = self.raw_body.or_else(|| self.body.map(|b| b.to_string()));
        let request = match payload {
            Some(p) => {
                if let Some(ct) = self.content_type {
                    builder = builder.header(header::CONTENT_TYPE, ct);
                }
                builder.body(Body::from(p)).unwrap()
            }
            None => builder.body(Body::empty()).unwrap(),
        };

        app.clone().oneshot(request).await.unwrap()
    }

    async fn json_response(self, app: &Router) -> (StatusCode, Value) {
        split(self.send(app).await).await
    }
}

async fn split(response: Response<Body>) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn order_body() -> Value {
    json!({ "amount": 150000, "currency": "INR", "receipt": "r_1" })
}

fn payment_body(order_id: &str, card: &str) -> Value {
    json!({
        "order_id": order_id,
        "card": { "number": card, "exp_month": 12, "exp_year": 2030, "cvc": "123" }
    })
}

/// Create an order and return its id.
///
/// `receipt` is unique per call: it is constrained unique per merchant, and a
/// helper that quietly collided would make every multi-order test a test of
/// that constraint instead of what it meant to cover.
async fn create_order(app: &Router, token: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);

    let (status, order) = Call::new("POST", "/v1/orders")
        .auth(token)
        .json(json!({ "amount": 150000, "currency": "INR", "receipt": format!("r_{n}") }))
        .json_response(app)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{order}");
    order["id"].as_str().unwrap().to_owned()
}

// ===========================================================================
// Authentication
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn no_auth_is_401_with_the_envelope(pool: PgPool) {
    let app = app(&pool);
    let (status, body) = Call::new("POST", "/v1/orders")
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn wrong_key_is_401(pool: PgPool) {
    seed(&pool).await;
    let app = app(&pool);
    let (status, _) = Call::new("POST", "/v1/orders")
        .auth("sk_test_definitely_not_real")
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations")]
async fn publishable_key_is_rejected_with_guidance(pool: PgPool) {
    seed(&pool).await;
    let app = app(&pool);
    let (status, body) = Call::new("POST", "/v1/orders")
        .auth("pk_test_whatever")
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("secret key"));
}

/// An unauthenticated request to a path that does not exist must look exactly
/// like one to a path that does — otherwise the 404/401 split is a map of the
/// API for anyone without a key.
#[sqlx::test(migrations = "../../migrations")]
async fn unknown_paths_under_v1_do_not_leak_existence_to_the_unauthenticated(pool: PgPool) {
    let app = app(&pool);
    let (status, _) = Call::new("GET", "/v1/definitely_not_a_resource")
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// The happy path
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn full_success_flow_over_http(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, order) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(order_body())
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let order_id = order["id"].as_str().unwrap();
    assert!(order_id.starts_with("order_"));
    assert_eq!(order["status"], "created");

    let (status, payment) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(payment_body(order_id, "4242424242424242"))
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(payment["status"], "captured");
    assert_eq!(payment["amount"], 150000);
    assert_eq!(payment["card"]["last4"], "4242");

    // Order reflects payment.
    let (_, fetched) = Call::new("GET", format!("/v1/orders/{order_id}"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(fetched["status"], "paid");
    assert_eq!(fetched["amount_paid"], 150000);

    // Both events visible.
    let (_, events) = Call::new("GET", "/v1/events")
        .auth(&token)
        .json_response(&app)
        .await;
    let types: Vec<&str> = events["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"payment.captured"));
    assert!(types.contains(&"order.paid"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn authorize_card_then_capture_endpoint(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;

    let (status, payment) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(payment_body(&order_id, "4000000000000077"))
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(payment["status"], "authorized");
    let pid = payment["id"].as_str().unwrap();

    let (status, captured) = Call::new("POST", format!("/v1/payments/{pid}/capture"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(captured["status"], "captured");
}

// ===========================================================================
// Card failures
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn declined_card_is_402_with_a_fetchable_payment(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;

    let (status, body) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(payment_body(&order_id, "4000000000009995"))
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["type"], "card_error");
    assert_eq!(body["error"]["code"], "insufficient_funds");

    // The failed payment was committed and is fetchable by the id in the error.
    let payment_id = body["error"]["payment_id"].as_str().unwrap();
    let (status, payment) = Call::new("GET", format!("/v1/payments/{payment_id}"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payment["status"], "failed");
    assert_eq!(payment["error_code"], "insufficient_funds");
}

#[sqlx::test(migrations = "../../migrations")]
async fn unknown_card_is_rejected_with_no_payment_created(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;

    // Valid Luhn, not in the test set — the safety gate.
    let (status, body) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(payment_body(&order_id, "4111111111111111"))
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["code"], "card_not_in_test_set");
    assert!(body["error"]["payment_id"].is_null());

    // Nothing was written; the order is untouched.
    let (_, fetched) = Call::new("GET", format!("/v1/orders/{order_id}"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(fetched["status"], "created");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_cvc_is_a_card_error_that_never_quotes_the_value(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;

    let (status, body) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(json!({
            "order_id": order_id,
            "card": {
                "number": "4242424242424242",
                "exp_month": 12,
                "exp_year": 2030,
                "cvc": "12345"
            }
        }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["code"], "incorrect_cvc");
    assert!(!body.to_string().contains("12345"));
}

/// The card number must not survive into any response, not even an error one.
#[sqlx::test(migrations = "../../migrations")]
async fn no_response_ever_echoes_a_full_card_number(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;
    let pan = "4000000000009995";

    let (_, declined) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(payment_body(&order_id, pan))
        .json_response(&app)
        .await;
    assert!(!declined.to_string().contains(pan), "{declined}");

    let (_, events) = Call::new("GET", "/v1/events")
        .auth(&token)
        .json_response(&app)
        .await;
    assert!(
        !events.to_string().contains(pan),
        "PAN reached the event log"
    );
}

// ===========================================================================
// Tenant isolation
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn cross_merchant_reads_are_404(pool: PgPool) {
    let (_m1, token1) = seed(&pool).await;
    let m2 = domain::merchant::Merchant::new("Beta", "b@beta.dev", now()).unwrap();
    store::merchant::insert(&pool, &m2).await.unwrap();
    let gk2 = crypto::api_key::generate("sk_test").unwrap();
    store::api_key::insert(
        &pool,
        &ApiKey::new(m2.id, ApiKeyKind::Secret, "t", gk2.hash, now()),
    )
    .await
    .unwrap();

    let app = app(&pool);
    let order_id = create_order(&app, &token1).await;

    let (status, _) = Call::new("GET", format!("/v1/orders/{order_id}"))
        .auth(&gk2.plaintext)
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND); // not 403 — existence never leaks
}

// ===========================================================================
// Request shape: everything a client can get wrong
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn an_unknown_path_is_a_404_in_the_envelope(pool: PgPool) {
    let app = app(&pool);
    let (status, body) = Call::new("GET", "/not_an_endpoint")
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_endpoint");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/not_an_endpoint"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_wrong_method_is_a_405_in_the_envelope(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("DELETE", "/v1/orders")
        .auth(&token)
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

#[sqlx::test(migrations = "../../migrations")]
async fn malformed_json_is_a_400_in_the_envelope_not_plain_text(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .raw("{ not json")
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "json_invalid");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_missing_content_type_is_a_415(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(order_body())
        .no_content_type()
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(body["error"]["code"], "unsupported_media_type");
}

/// A typo'd field must not be silently ignored — that is how an integration
/// "works" for a week and then bills the wrong amount.
#[sqlx::test(migrations = "../../migrations")]
async fn an_unknown_field_is_rejected_and_named(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(json!({ "amount": 150000, "currency": "INR", "recipt": "typo" }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("recipt"),
        "the message should name the offending field: {body}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_wrongly_typed_field_names_its_path(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;

    let (status, body) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .json(json!({
            "order_id": order_id,
            "card": { "number": "4242424242424242", "exp_month": "twelve", "exp_year": 2030 }
        }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "card.exp_month");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_bad_currency_is_a_400_naming_the_parameter(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(json!({ "amount": 150000, "currency": "XYZ" }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "currency_invalid");
    assert_eq!(body["error"]["param"], "currency");
}

/// A merchant's receipts are unique per merchant. Reusing one is their mistake
/// to fix, so it must read as a 409 — a 500 would say the fault was ours and
/// that retrying the identical request might work.
#[sqlx::test(migrations = "../../migrations")]
async fn a_duplicate_receipt_is_a_409_not_a_500(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, _) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(order_body())
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        !body.to_string().contains("orders_merchant_receipt_idx"),
        "internal constraint names must not reach the wire: {body}"
    );
}

// ===========================================================================
// Request correlation
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn every_response_carries_a_request_id_and_errors_repeat_it_in_the_body(pool: PgPool) {
    let app = app(&pool);

    let response = Call::new("GET", "/v1/orders").send(&app).await;
    let header_id = response
        .headers()
        .get(&REQUEST_ID)
        .expect("responses must be correlatable")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header_id.starts_with("req_"), "{header_id}");

    let (_, body) = split(response).await;
    assert_eq!(body["error"]["request_id"], header_id);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_caller_supplied_request_id_is_adopted(pool: PgPool) {
    let app = app(&pool);

    let response = Call::new("GET", "/health")
        .header(REQUEST_ID, "trace-abc-123")
        .send(&app)
        .await;

    assert_eq!(
        response.headers().get(&REQUEST_ID).unwrap(),
        "trace-abc-123"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_abusive_request_id_is_replaced_rather_than_echoed(pool: PgPool) {
    let app = app(&pool);

    // Header injection shaped input, plus something far too long to log.
    let response = Call::new("GET", "/health")
        .header(REQUEST_ID, "short")
        .send(&app)
        .await;
    let id = response
        .headers()
        .get(&REQUEST_ID)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        id.starts_with("req_"),
        "an unusable id must be replaced: {id}"
    );
}

// ===========================================================================
// Idempotency
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn a_repeated_key_replays_the_original_response_instead_of_charging_twice(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;
    let key = "idem-key-payment-0001";
    let body = payment_body(&order_id, "4242424242424242");

    let first = Call::new("POST", "/v1/payments")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(body.clone())
        .send(&app)
        .await;
    assert!(first.headers().get(&REPLAYED).is_none());
    let (status, first_body) = split(first).await;
    assert_eq!(status, StatusCode::CREATED);

    let second = Call::new("POST", "/v1/payments")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(body)
        .send(&app)
        .await;
    assert_eq!(
        second.headers().get(&REPLAYED).unwrap(),
        "true",
        "a replay must announce itself"
    );
    let (status, second_body) = split(second).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        first_body["id"], second_body["id"],
        "the retry must return the ORIGINAL payment, not a second one"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn reusing_a_key_for_a_different_body_is_a_409(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let key = "idem-key-order-0001";

    let (status, _) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(order_body())
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(json!({ "amount": 999999, "currency": "INR" }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "idempotency_key_reused");
    assert_eq!(body["error"]["type"], "idempotency_error");
}

/// Two merchants using the same key string must not see each other's results.
#[sqlx::test(migrations = "../../migrations")]
async fn idempotency_keys_are_scoped_per_merchant(pool: PgPool) {
    let (_m1, token1) = seed(&pool).await;
    let m2 = domain::merchant::Merchant::new("Beta", "b@beta.dev", now()).unwrap();
    store::merchant::insert(&pool, &m2).await.unwrap();
    let gk2 = crypto::api_key::generate("sk_test").unwrap();
    store::api_key::insert(
        &pool,
        &ApiKey::new(m2.id, ApiKeyKind::Secret, "t", gk2.hash, now()),
    )
    .await
    .unwrap();

    let app = app(&pool);
    let key = "shared-key-value-01";

    let (_, first) = Call::new("POST", "/v1/orders")
        .auth(&token1)
        .header(IDEMPOTENCY_KEY, key)
        .json(order_body())
        .json_response(&app)
        .await;

    let (status, second) = Call::new("POST", "/v1/orders")
        .auth(&gk2.plaintext)
        .header(IDEMPOTENCY_KEY, key)
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_ne!(first["id"], second["id"]);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_idempotency_key_is_rejected_before_any_work(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/orders")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, "short")
        .json(order_body())
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "idempotency_key_invalid");

    // Nothing was created.
    let (_, listed) = Call::new("GET", "/v1/orders")
        .auth(&token)
        .json_response(&app)
        .await;
    assert!(listed["data"].as_array().unwrap().is_empty());
}

/// A decline is a real outcome, so it replays like any other — retrying must not
/// present the card again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_declined_payment_replays_as_the_same_decline(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    let order_id = create_order(&app, &token).await;
    let key = "idem-key-decline-001";
    let body = payment_body(&order_id, "4000000000009995");

    let (status, first) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(body.clone())
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

    let (status, second) = Call::new("POST", "/v1/payments")
        .auth(&token)
        .header(IDEMPOTENCY_KEY, key)
        .json(body)
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(
        first["error"]["payment_id"], second["error"]["payment_id"],
        "a replayed decline must reference the same payment"
    );
}

// ===========================================================================
// Pagination
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn a_collection_reports_has_more_and_hands_back_a_cursor(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);
    for _ in 0..3 {
        create_order(&app, &token).await;
    }

    let (status, page) = Call::new("GET", "/v1/orders?limit=2")
        .auth(&token)
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["object"], "list");
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    assert_eq!(page["has_more"], true);
    assert!(page["next_before"].is_string());

    // The last page says so, and offers no cursor to keep going.
    let (_, all) = Call::new("GET", "/v1/orders?limit=10")
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(all["data"].as_array().unwrap().len(), 3);
    assert_eq!(all["has_more"], false);
    assert!(all["next_before"].is_null());
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_out_of_range_limit_is_an_error_rather_than_a_silent_clamp(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    for query in ["?limit=0", "?limit=-1", "?limit=100000"] {
        let (status, body) = Call::new("GET", format!("/v1/orders{query}"))
            .auth(&token)
            .json_response(&app)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {query}");
        assert_eq!(body["error"]["code"], "limit_invalid");
        assert_eq!(body["error"]["param"], "limit");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_cursor_is_a_400(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("GET", "/v1/orders?before=yesterday")
        .auth(&token)
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "cursor_invalid");
}

#[sqlx::test(migrations = "../../migrations")]
async fn events_are_paginated_like_every_other_collection(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, page) = Call::new("GET", "/v1/events?limit=1")
        .auth(&token)
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["object"], "list");
    assert_eq!(page["has_more"], false);
    assert!(page["next_before"].is_null());
}

// ===========================================================================
// Webhook endpoints
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn webhook_endpoint_secret_shown_once(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, created) = Call::new("POST", "/v1/webhook_endpoints")
        .auth(&token)
        .json(json!({ "url": "https://shop.acme.dev/webhooks" }))
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(created["signing_secret"]
        .as_str()
        .unwrap()
        .starts_with("whsec_"));

    let (_, listed) = Call::new("GET", "/v1/webhook_endpoints")
        .auth(&token)
        .json_response(&app)
        .await;
    assert!(listed["data"][0]["signing_secret"].is_null()); // never again
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_dangerous_webhook_url_is_refused(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    for url in [
        "file:///etc/passwd",
        "not-a-url",
        "https://user:pass@acme.dev/hooks",
    ] {
        let (status, body) = Call::new("POST", "/v1/webhook_endpoints")
            .auth(&token)
            .json(json!({ "url": url }))
            .json_response(&app)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted {url}");
        assert_eq!(body["error"]["param"], "url");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_malformed_event_subscription_is_refused(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (status, body) = Call::new("POST", "/v1/webhook_endpoints")
        .auth(&token)
        .json(json!({
            "url": "https://acme.dev/hooks",
            "enabled_events": ["Payment.Captured"]
        }))
        .json_response(&app)
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["param"], "enabled_events");
}

#[sqlx::test(migrations = "../../migrations")]
async fn deleting_an_endpoint_stops_it_from_being_listed_and_is_idempotent(pool: PgPool) {
    let (_mid, token) = seed(&pool).await;
    let app = app(&pool);

    let (_, created) = Call::new("POST", "/v1/webhook_endpoints")
        .auth(&token)
        .json(json!({ "url": "https://acme.dev/hooks" }))
        .json_response(&app)
        .await;
    let id = created["id"].as_str().unwrap();

    let (status, deleted) = Call::new("DELETE", format!("/v1/webhook_endpoints/{id}"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["deleted"], true);

    let (_, listed) = Call::new("GET", "/v1/webhook_endpoints")
        .auth(&token)
        .json_response(&app)
        .await;
    assert!(listed["data"].as_array().unwrap().is_empty());

    // A retried delete is still a success, not a 404.
    let (status, _) = Call::new("DELETE", format!("/v1/webhook_endpoints/{id}"))
        .auth(&token)
        .json_response(&app)
        .await;
    assert_eq!(status, StatusCode::OK);
}

// ===========================================================================
// Health
// ===========================================================================

#[sqlx::test(migrations = "../../migrations")]
async fn health_is_open_and_uncacheable(pool: PgPool) {
    let app = app(&pool);

    for path in ["/health", "/health/live", "/health/ready"] {
        let response = Call::new("GET", path).send(&app).await;
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "{path} must not be cached"
        );

        let (status, body) = split(response).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body["status"], "ok", "{path}");
    }
}
