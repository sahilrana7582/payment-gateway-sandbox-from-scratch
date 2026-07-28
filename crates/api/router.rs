//! Route assembly. Auth wraps everything under /v1; /health stays open.

use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::handlers;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    let authed = Router::new()
        .route("/orders", post(handlers::create_order).get(handlers::list_orders))
        .route("/orders/{id}", get(handlers::get_order))
        .route("/payments", post(handlers::create_payment))
        .route("/payments/{id}", get(handlers::get_payment))
        .route("/payments/{id}/capture", post(handlers::capture_payment))
        .route(
            "/webhook_endpoints",
            post(handlers::create_webhook_endpoint).get(handlers::list_webhook_endpoints),
        )
        .route("/events", get(handlers::list_events))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .route("/health", get(handlers::health))
        .nest("/v1", authed)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use domain::api_key::{ApiKey, ApiKeyKind};
    use domain::clock::FixedClock;
    use domain::id::MerchantId;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use time::macros::datetime;
    use tower::ServiceExt;

    fn now() -> time::OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    fn app(pool: &PgPool) -> Router {
        router(AppState::new(
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

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, json)
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

    #[sqlx::test(migrations = "../../migrations")]
    async fn no_auth_is_401_with_the_envelope(pool: PgPool) {
        let app = app(&pool);
        let (status, body) = call(&app, "POST", "/v1/orders", None, Some(order_body())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"], "authentication_error");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn wrong_key_is_401(pool: PgPool) {
        seed(&pool).await;
        let app = app(&pool);
        let (status, _) = call(
            &app, "POST", "/v1/orders",
            Some("sk_test_definitely_not_real"), Some(order_body()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn publishable_key_is_rejected_with_guidance(pool: PgPool) {
        seed(&pool).await;
        let app = app(&pool);
        let (status, body) = call(
            &app, "POST", "/v1/orders",
            Some("pk_test_whatever"), Some(order_body()),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"]["message"].as_str().unwrap().contains("secret key"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn full_success_flow_over_http(pool: PgPool) {
        let (_mid, token) = seed(&pool).await;
        let app = app(&pool);

        let (status, order) =
            call(&app, "POST", "/v1/orders", Some(&token), Some(order_body())).await;
        assert_eq!(status, StatusCode::CREATED);
        let order_id = order["id"].as_str().unwrap();
        assert!(order_id.starts_with("order_"));
        assert_eq!(order["status"], "created");

        let (status, payment) = call(
            &app, "POST", "/v1/payments", Some(&token),
            Some(payment_body(order_id, "4242424242424242")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(payment["status"], "captured");
        assert_eq!(payment["amount"], 150000);
        assert_eq!(payment["card"]["last4"], "4242");

        // Order reflects payment.
        let (_, fetched) =
            call(&app, "GET", &format!("/v1/orders/{order_id}"), Some(&token), None).await;
        assert_eq!(fetched["status"], "paid");
        assert_eq!(fetched["amount_paid"], 150000);

        // Both events visible.
        let (_, events) = call(&app, "GET", "/v1/events", Some(&token), None).await;
        let types: Vec<&str> = events["data"]
            .as_array().unwrap()
            .iter().map(|e| e["type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"payment.captured"));
        assert!(types.contains(&"order.paid"));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn declined_card_is_402_with_a_fetchable_payment(pool: PgPool) {
        let (_mid, token) = seed(&pool).await;
        let app = app(&pool);

        let (_, order) =
            call(&app, "POST", "/v1/orders", Some(&token), Some(order_body())).await;
        let order_id = order["id"].as_str().unwrap();

        let (status, body) = call(
            &app, "POST", "/v1/payments", Some(&token),
            Some(payment_body(order_id, "4000000000009995")),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(body["error"]["type"], "card_error");
        assert_eq!(body["error"]["code"], "insufficient_funds");

        // The failed payment was committed and is fetchable by the id in the error.
        let payment_id = body["error"]["payment_id"].as_str().unwrap();
        let (status, payment) = call(
            &app, "GET", &format!("/v1/payments/{payment_id}"), Some(&token), None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payment["status"], "failed");
        assert_eq!(payment["error_code"], "insufficient_funds");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn unknown_card_is_rejected_with_no_payment_created(pool: PgPool) {
        let (_mid, token) = seed(&pool).await;
        let app = app(&pool);

        let (_, order) =
            call(&app, "POST", "/v1/orders", Some(&token), Some(order_body())).await;
        let order_id = order["id"].as_str().unwrap();

        // Valid Luhn, not in the test set — the safety gate.
        let (status, body) = call(
            &app, "POST", "/v1/payments", Some(&token),
            Some(payment_body(order_id, "4111111111111111")),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(body["error"]["code"], "card_not_in_test_set");
        assert!(body["error"]["payment_id"].is_null());

        // Nothing was written; the order is untouched.
        let (_, fetched) =
            call(&app, "GET", &format!("/v1/orders/{order_id}"), Some(&token), None).await;
        assert_eq!(fetched["status"], "created");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn authorize_card_then_capture_endpoint(pool: PgPool) {
        let (_mid, token) = seed(&pool).await;
        let app = app(&pool);

        let (_, order) =
            call(&app, "POST", "/v1/orders", Some(&token), Some(order_body())).await;
        let order_id = order["id"].as_str().unwrap();

        let (status, payment) = call(
            &app, "POST", "/v1/payments", Some(&token),
            Some(payment_body(order_id, "4000000000000077")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(payment["status"], "authorized");
        let pid = payment["id"].as_str().unwrap();

        let (status, captured) = call(
            &app, "POST", &format!("/v1/payments/{pid}/capture"), Some(&token), None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(captured["status"], "captured");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn webhook_endpoint_secret_shown_once(pool: PgPool) {
        let (_mid, token) = seed(&pool).await;
        let app = app(&pool);

        let (status, created) = call(
            &app, "POST", "/v1/webhook_endpoints", Some(&token),
            Some(json!({ "url": "https://shop.acme.dev/webhooks" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(created["signing_secret"].as_str().unwrap().starts_with("whsec_"));

        let (_, listed) = call(&app, "GET", "/v1/webhook_endpoints", Some(&token), None).await;
        assert!(listed["data"][0]["signing_secret"].is_null()); // never again
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn cross_merchant_reads_are_404(pool: PgPool) {
        let (_m1, token1) = seed(&pool).await;
        // Second merchant.
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
        let (_, order) =
            call(&app, "POST", "/v1/orders", Some(&token1), Some(order_body())).await;
        let order_id = order["id"].as_str().unwrap();

        let (status, _) = call(
            &app, "GET", &format!("/v1/orders/{order_id}"),
            Some(&gk2.plaintext), None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND); // not 403 — existence never leaks
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn health_is_open(pool: PgPool) {
        let app = app(&pool);
        let (status, body) = call(&app, "GET", "/health", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
    }
}