use std::time::Instant;

use domain::clock::Clock;
use serde_json::json;
use store::{event::Event, webhook_endpoint::WebhookEndpoint};
use time::format_description::well_known::Rfc3339;

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub timeout: std::time::Duration,

    pub max_response_bytes: usize,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(10),
            max_response_bytes: 2048,
        }
    }
}

/// The full webhook envelope. Defined here and nowhere else — the event row
/// stores only `data`; the envelope shape has exactly one source of truth.
pub fn build_envelope(event: &Event) -> serde_json::Value {
    json!({
        "id": event.id.to_string(),
        "type": event.event_type,
        "created_at": event.created_at.format(&Rfc3339).unwrap_or_default(),
        "api_version": event.api_version,
        "data": event.payload,
    })
}

/// What happened when we tried to deliver.
#[derive(Debug)]
pub enum SendOutcome {
    /// 2xx received.
    Delivered {
        status: i32,
        body: String,
        duration_ms: i32,
    },
    /// Non-2xx, or a transport error (connect refused, timeout).
    Failed {
        status: Option<i32>,
        body: Option<String>,
        duration_ms: i32,
        error: String,
    },
}

/// Sign and send one event to one endpoint.
pub async fn send(
    client: &reqwest::Client,
    config: &WebhookConfig,
    clock: &dyn Clock,
    endpoint: &WebhookEndpoint,
    event: &Event,
) -> SendOutcome {
    let body = build_envelope(event).to_string();
    let timestamp = clock.unix_timestamp();
    let signature =
        crypto::signing::build_header(&endpoint.signing_secret, timestamp, body.as_bytes());

    let started = Instant::now();

    let result = client
        .post(&endpoint.url)
        .timeout(config.timeout)
        .header("Content-Type", "application/json")
        .header("X-Sandbox-Signature", signature)
        .header("X-Sandbox-Event-Id", event.id.to_string())
        .header("X-Sandbox-Event-Type", &event.event_type)
        .header("User-Agent", "payment-sandbox-webhooks/0.1")
        .body(body)
        .send()
        .await;

    let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);

    match result {
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            // The request timeout bounds this read; truncate before storing.
            let bytes = resp.bytes().await.unwrap_or_default();
            let truncated = &bytes[..bytes.len().min(config.max_response_bytes)];
            let body_text = String::from_utf8_lossy(truncated).to_string();

            if (200..300).contains(&status) {
                SendOutcome::Delivered {
                    status,
                    body: body_text,
                    duration_ms,
                }
            } else {
                SendOutcome::Failed {
                    status: Some(status),
                    body: Some(body_text),
                    duration_ms,
                    error: format!("endpoint returned {status}"),
                }
            }
        }
        Err(e) => SendOutcome::Failed {
            status: None,
            body: None,
            duration_ms,
            error: if e.is_timeout() {
                "request timed out".to_string()
            } else {
                format!("transport error: {e}")
            },
        },
    }
}
