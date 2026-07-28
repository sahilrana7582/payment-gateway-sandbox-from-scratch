//! The `deliver_webhook` job handler.
//!
//! # Who owns the retry
//!
//! The QUEUE owns retry scheduling: this handler returns `Retryable` and the
//! queue worker computes the next run from its own attempt count. The delivery
//! row's `attempt` / `next_retry_at` are the merchant-visible mirror of that
//! state for the dashboard. The two stay in lockstep because each delivery has
//! exactly one job, so `job.attempts` and `delivery.attempt` count the same
//! executions — both start at 1 on first claim, both use the same ladder.

use std::sync::Arc;

use domain::clock::Clock;
use domain::id::{EventId, WebhookEndpointId};
use queue::job::{kinds, BoxJobFuture, JobError, JobHandler};
use serde::Deserialize;
use serde_json::Value as Json;
use sqlx::PgPool;
use store::webhook_delivery::DeliveryStatus;
use tracing::{info, warn};
use uuid::Uuid;

use crate::delivery::{send, SendOutcome, WebhookConfig};

#[derive(Debug, Deserialize)]
struct DeliverPayload {
    delivery_id: Uuid,
    event_id: String,
    endpoint_id: String,
}

pub struct WebhookDeliveryHandler {
    clock: Arc<dyn Clock>,
    client: reqwest::Client,
    config: WebhookConfig,
}

impl WebhookDeliveryHandler {
    pub fn new(clock: Arc<dyn Clock>, config: WebhookConfig) -> Self {
        Self {
            clock,
            client: reqwest::Client::new(),
            config,
        }
    }

    async fn deliver(&self, pool: &PgPool, payload: &Json) -> Result<(), JobError> {
        // A payload we can't parse will never parse — permanent.
        let parsed: DeliverPayload = serde_json::from_value(payload.clone())
            .map_err(|e| JobError::Permanent(format!("malformed payload: {e}")))?;
        let event_id: EventId = parsed
            .event_id
            .parse()
            .map_err(|_| JobError::Permanent("bad event_id".into()))?;
        let endpoint_id: WebhookEndpointId = parsed
            .endpoint_id
            .parse()
            .map_err(|_| JobError::Permanent("bad endpoint_id".into()))?;

        let now = self.clock.now();

        let event = store::event::find_by_id(pool, event_id)
            .await
            .map_err(|e| JobError::Permanent(format!("event missing: {e}")))?;
        let endpoint = store::webhook_endpoint::find_by_id(pool, endpoint_id)
            .await
            .map_err(|e| JobError::Permanent(format!("endpoint missing: {e}")))?;
        let delivery = store::webhook_delivery::find_by_id(pool, parsed.delivery_id)
            .await
            .map_err(|e| JobError::Permanent(format!("delivery row missing: {e}")))?;

        // Endpoint disabled after enqueue: nothing to deliver to, ever.
        if !endpoint.is_active() {
            let _ = store::webhook_delivery::record_attempt(
                pool,
                delivery.id,
                DeliveryStatus::Dead,
                None,
                Some("endpoint disabled"),
                0,
                None,
                delivery.attempt,
                now,
            )
            .await;
            return Err(JobError::Permanent("endpoint disabled".into()));
        }

        let attempt = delivery.attempt;

        match send(
            &self.client,
            &self.config,
            self.clock.as_ref(),
            &endpoint,
            &event,
        )
        .await
        {
            SendOutcome::Delivered {
                status,
                body,
                duration_ms,
            } => {
                info!(
                    event = %event.event_type, endpoint = %endpoint.url,
                    status, attempt, "webhook delivered"
                );
                store::webhook_delivery::record_attempt(
                    pool,
                    delivery.id,
                    DeliveryStatus::Delivered,
                    Some(status),
                    Some(&body),
                    duration_ms,
                    None,
                    attempt,
                    now,
                )
                .await
                .map_err(|e| JobError::Retryable(format!("failed to record delivery: {e}")))?;
                Ok(())
            }
            SendOutcome::Failed {
                status,
                body,
                duration_ms,
                error,
            } => {
                // Mirror the queue's ladder into the merchant-visible row.
                let next_retry = queue::scheduler::next_retry_at(attempt, now);
                let (row_status, next_attempt) = match next_retry {
                    Some(_) => (DeliveryStatus::Failed, attempt + 1),
                    None => (DeliveryStatus::Dead, attempt),
                };

                warn!(
                    event = %event.event_type, endpoint = %endpoint.url,
                    ?status, attempt, error = %error, "webhook delivery failed"
                );

                let _ = store::webhook_delivery::record_attempt(
                    pool,
                    delivery.id,
                    row_status,
                    status,
                    body.as_deref(),
                    duration_ms,
                    next_retry,
                    next_attempt,
                    now,
                )
                .await;

                // The queue computes the authoritative reschedule (or death)
                // from its own attempt count, which matches ours.
                Err(JobError::Retryable(error))
            }
        }
    }
}

impl JobHandler for WebhookDeliveryHandler {
    fn kind(&self) -> &'static str {
        kinds::DELIVER_WEBHOOK
    }

    fn handle<'a>(&'a self, pool: &'a PgPool, payload: &'a Json) -> BoxJobFuture<'a> {
        Box::pin(self.deliver(pool, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::clock::FixedClock;
    use domain::id::MerchantId;
    use domain::money::Currency;
    use engine::{
        AttemptDecision, AttemptOutcome, CreateOrderInput, CreatePaymentInput, OrderService,
        PaymentService,
    };
    use queue::worker::{Worker, WorkerConfig};
    use std::collections::HashMap;
    use time::macros::datetime;
    use wiremock::matchers::{header_exists, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn now() -> time::OffsetDateTime {
        datetime!(2026-01-01 00:00:00 UTC)
    }

    fn clock() -> Arc<FixedClock> {
        Arc::new(FixedClock::new(now()))
    }

    async fn seed_merchant_with_endpoint(pool: &PgPool, url: &str, secret: &str) -> MerchantId {
        let m = domain::merchant::Merchant::new("Acme", "a@acme.dev", now()).unwrap();
        store::merchant::insert(pool, &m).await.unwrap();
        let ep = store::webhook_endpoint::WebhookEndpoint {
            id: WebhookEndpointId::new(),
            merchant_id: m.id,
            url: url.to_string(),
            signing_secret: secret.to_string(),
            enabled_events: vec![],
            disabled_at: None,
            created_at: now(),
        };
        store::webhook_endpoint::insert(pool, &ep).await.unwrap();
        m.id
    }

    /// Drive a captured payment through engine, then pump the queue once, and
    /// assert the mock merchant endpoint received BOTH events with signatures
    /// that verify against the exact received bodies. This is the walking
    /// skeleton, minus only the HTTP api layer.
    #[sqlx::test(migrations = "../../migrations")]
    async fn full_loop_payment_to_verified_webhook(pool: PgPool) {
        let server = MockServer::start().await;
        let secret = "whsec_full_loop";
        Mock::given(method("POST"))
            .and(header_exists("X-Sandbox-Signature"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .expect(2) // payment.captured + order.paid
            .mount(&server)
            .await;

        let mid = seed_merchant_with_endpoint(&pool, &server.uri(), secret).await;

        let orders = OrderService::new(pool.clone(), clock());
        let order = orders
            .create(
                mid,
                CreateOrderInput {
                    amount_minor: 150_000,
                    currency: Currency::Inr,
                    receipt: None,
                    notes: HashMap::new(),
                },
            )
            .await
            .unwrap();

        let payments = PaymentService::new(pool.clone(), clock(), "pepper".into());
        payments
            .create(
                mid,
                CreatePaymentInput {
                    order_id: order.id,
                    card_number: "4242424242424242".into(),
                    card_exp_month: 12,
                    card_exp_year: 2030,
                    notes: HashMap::new(),
                },
                AttemptDecision {
                    outcome: AttemptOutcome::Capture,
                    latency_ms: 400,
                },
            )
            .await
            .unwrap();

        let mut worker = Worker::new(pool.clone(), clock(), WorkerConfig::default());
        worker.register(Arc::new(WebhookDeliveryHandler::new(
            clock(),
            WebhookConfig::default(),
        )));
        worker.tick().await.unwrap();

        // Signature on every received request verifies against its exact body.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for req in &requests {
            let sig = req
                .headers
                .get("X-Sandbox-Signature")
                .unwrap()
                .to_str()
                .unwrap();
            crypto::signing::verify(
                secret,
                sig,
                &req.body,
                now().unix_timestamp(),
                crypto::signing::DEFAULT_TOLERANCE_SECS,
            )
            .expect("signature must verify against the received body");
        }

        // Delivery log shows both as delivered.
        let events = store::event::list_for_merchant(&pool, mid, None, 10)
            .await
            .unwrap();
        for ev in &events {
            let log = store::webhook_delivery::list_for_event(&pool, ev.id)
                .await
                .unwrap();
            assert!(log.iter().all(|d| d.status == DeliveryStatus::Delivered));
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_500_marks_failed_and_schedules_the_mirror_retry(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let mid = seed_merchant_with_endpoint(&pool, &server.uri(), "whsec_x").await;

        // Emit one event directly.
        let endpoints = store::webhook_endpoint::list_active_for_merchant(&pool, mid)
            .await
            .unwrap();
        let mut tx = store::tx::begin(&pool).await.unwrap();
        engine::event::emit(
            &mut tx,
            &endpoints,
            mid,
            "payment.captured",
            serde_json::json!({"x": 1}),
            now(),
        )
        .await
        .unwrap();
        store::tx::commit(tx).await.unwrap();

        let mut worker = Worker::new(pool.clone(), clock(), WorkerConfig::default());
        worker.register(Arc::new(WebhookDeliveryHandler::new(
            clock(),
            WebhookConfig::default(),
        )));
        worker.tick().await.unwrap();

        let events = store::event::list_for_merchant(&pool, mid, None, 10)
            .await
            .unwrap();
        let log = store::webhook_delivery::list_for_event(&pool, events[0].id)
            .await
            .unwrap();
        assert_eq!(log[0].status, DeliveryStatus::Failed);
        assert_eq!(log[0].response_status, Some(500));
        assert_eq!(log[0].attempt, 2); // next attempt number
                                       // Mirror matches the queue ladder: first retry lands +1 minute out.
        assert_eq!(
            log[0].next_retry_at,
            Some(now() + time::Duration::minutes(1))
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn timeout_is_retryable(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(2)))
            .mount(&server)
            .await;

        let mid = seed_merchant_with_endpoint(&pool, &server.uri(), "whsec_x").await;
        let endpoints = store::webhook_endpoint::list_active_for_merchant(&pool, mid)
            .await
            .unwrap();
        let mut tx = store::tx::begin(&pool).await.unwrap();
        engine::event::emit(
            &mut tx,
            &endpoints,
            mid,
            "order.paid",
            serde_json::json!({}),
            now(),
        )
        .await
        .unwrap();
        store::tx::commit(tx).await.unwrap();

        // 500ms timeout against a 2s responder.
        let mut worker = Worker::new(pool.clone(), clock(), WorkerConfig::default());
        worker.register(Arc::new(WebhookDeliveryHandler::new(
            clock(),
            WebhookConfig {
                timeout: std::time::Duration::from_millis(500),
                max_response_bytes: 2048,
            },
        )));
        worker.tick().await.unwrap();

        let events = store::event::list_for_merchant(&pool, mid, None, 10)
            .await
            .unwrap();
        let log = store::webhook_delivery::list_for_event(&pool, events[0].id)
            .await
            .unwrap();
        assert_eq!(log[0].status, DeliveryStatus::Failed);
        assert_eq!(log[0].response_status, None); // never got a status
    }
}
