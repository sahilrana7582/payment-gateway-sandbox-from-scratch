//! Edge validation — the checks that belong to the HTTP layer specifically.
//!
//! The division of labour is deliberate and worth stating, because "where does
//! validation live" is the question that produces the most duplicated code in
//! services like this one:
//!
//! * **`domain`** owns what is true of a value forever (a Luhn-valid card
//!   number, a non-negative amount).
//! * **`engine`** owns business rules (minimum order amount, note limits) so
//!   they hold no matter which transport called in.
//! * **here** owns whatever is only meaningful because a request arrived over
//!   HTTP: header shapes, URL syntax, string sizes that exist to bound work
//!   rather than to express a rule.
//!
//! Nothing in this file re-checks something `engine` already guarantees. Where
//! that seems tempting, the right move is a better error message out of the
//! engine, not a second copy of the rule that will drift.

use std::collections::HashSet;

use url::Url;

use crate::error::{ApiError, ApiResult};

/// Longest webhook URL we will store. Comfortably past any real endpoint and
/// short enough to keep the delivery worker's logs readable.
const MAX_URL_LEN: usize = 2048;

/// Cap on a single endpoint's event subscription list.
const MAX_ENABLED_EVENTS: usize = 32;
const MAX_EVENT_TYPE_LEN: usize = 64;

/// Bounds on an `Idempotency-Key`. The lower bound rejects keys with too little
/// entropy to be unique (`"1"`); the upper bound stops the key column from
/// becoming a place to store data.
const MIN_IDEMPOTENCY_KEY_LEN: usize = 8;
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// Validate a card security code without ever storing or logging it.
///
/// The value is checked for shape only and then dropped by the caller — a CVC
/// is never persisted anywhere, which is a hard PCI rule rather than a
/// preference. Errors are 402 card errors rather than 400s because the card
/// data is what is wrong.
pub fn cvc(raw: &str) -> ApiResult<()> {
    let plausible = (3..=4).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_digit());
    if plausible {
        Ok(())
    } else {
        // The message never quotes the value back.
        Err(ApiError::card(
            "incorrect_cvc",
            "The security code must be 3 or 4 digits.",
            None,
        ))
    }
}

/// Validate a merchant-supplied webhook URL and return its normalised form.
///
/// # On SSRF
///
/// The delivery worker will make outbound requests to whatever this returns, so
/// this is the crate's one genuine server-side request forgery surface. A
/// production gateway resolves the host and refuses loopback, link-local and
/// RFC1918 ranges. This sandbox deliberately does **not**: its entire purpose is
/// letting a developer point a webhook at `http://localhost:4000` and watch it
/// arrive. The mitigations that remain are the ones that cost nothing to keep —
/// scheme allow-listing (no `file:`, no `gopher:`), rejecting embedded
/// credentials, and bounding the length. A deployment of this code that is
/// reachable by untrusted merchants must add range filtering in the delivery
/// worker, where the resolved address is actually known.
pub fn webhook_url(raw: &str) -> ApiResult<String> {
    let invalid = |message: &str| ApiError::invalid_request("url_invalid", message, Some("url"));

    if raw.len() > MAX_URL_LEN {
        return Err(invalid(&format!(
            "Webhook URL must be at most {MAX_URL_LEN} characters."
        )));
    }

    let url = Url::parse(raw)
        .map_err(|_| invalid("Webhook URL is not a valid absolute URL, e.g. https://example.com/hooks."))?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("Webhook URL must use the http or https scheme."));
    }

    // `Url` treats a missing host as valid for some schemes; for ours it is not.
    if url.host().is_none() {
        return Err(invalid("Webhook URL must include a host."));
    }

    // Credentials in the URL would be written to the endpoint row and echoed in
    // every delivery log line. Merchants who need auth use the signature.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "Webhook URL must not contain embedded credentials. Verify deliveries \
             with the signing secret instead.",
        ));
    }

    // A fragment is never sent over the wire, so accepting one stores a URL that
    // is not the URL we will call.
    if url.fragment().is_some() {
        return Err(invalid("Webhook URL must not contain a fragment."));
    }

    Ok(url.to_string())
}

/// Validate an event subscription list.
///
/// Types are checked for *shape* (`resource.action`) rather than against a fixed
/// allow-list. An allow-list here would have to be updated in lockstep with
/// every new event the engine learns to emit, and the failure mode when someone
/// forgets is that a merchant cannot subscribe to a real event — worse than the
/// failure mode of a typo'd subscription, which is a webhook that never fires
/// and is obvious in the dashboard.
pub fn enabled_events(events: &[String]) -> ApiResult<()> {
    let invalid = |message: String| {
        ApiError::invalid_request("enabled_events_invalid", message, Some("enabled_events"))
    };

    if events.len() > MAX_ENABLED_EVENTS {
        return Err(invalid(format!(
            "At most {MAX_ENABLED_EVENTS} event types may be enabled."
        )));
    }

    let mut seen = HashSet::with_capacity(events.len());
    for event in events {
        if !seen.insert(event.as_str()) {
            return Err(invalid(format!("Duplicate event type '{event}'.")));
        }

        let shaped = event.len() <= MAX_EVENT_TYPE_LEN
            && event.split_once('.').is_some_and(|(resource, action)| {
                !resource.is_empty()
                    && !action.is_empty()
                    && event
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || matches!(b, b'.' | b'_'))
            });

        if !shaped {
            return Err(invalid(format!(
                "'{}' is not a valid event type. Expected lowercase 'resource.action', \
                 e.g. 'payment.captured'.",
                truncate(event, MAX_EVENT_TYPE_LEN)
            )));
        }
    }

    Ok(())
}

/// Validate an `Idempotency-Key` header value.
///
/// Restricted to printable ASCII: the value is stored, indexed, and logged, and
/// a key containing a newline turns one log line into two.
pub fn idempotency_key(raw: &str) -> ApiResult<&str> {
    let well_formed = (MIN_IDEMPOTENCY_KEY_LEN..=MAX_IDEMPOTENCY_KEY_LEN).contains(&raw.len())
        && raw.bytes().all(|b| b.is_ascii_graphic());

    if well_formed {
        Ok(raw)
    } else {
        Err(ApiError::invalid_request(
            "idempotency_key_invalid",
            format!(
                "'Idempotency-Key' must be {MIN_IDEMPOTENCY_KEY_LEN}-{MAX_IDEMPOTENCY_KEY_LEN} \
                 printable ASCII characters. A UUID is a good choice."
            ),
            None,
        ))
    }
}

/// Bound a string before it goes into an error message, on a character boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).chain(std::iter::once('…')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    // -- cvc -----------------------------------------------------------------

    #[test]
    fn accepts_three_and_four_digit_codes() {
        assert!(cvc("123").is_ok());
        assert!(cvc("1234").is_ok());
    }

    #[test]
    fn rejects_malformed_codes_as_card_errors() {
        for bad in ["", "12", "12345", "12a", " 123", "١٢٣"] {
            let err = cvc(bad).unwrap_err();
            assert_eq!(err.status, StatusCode::PAYMENT_REQUIRED, "accepted {bad:?}");
        }
    }

    #[test]
    fn the_cvc_is_never_quoted_back() {
        let err = cvc("99999").unwrap_err();
        assert!(!err.message.contains("99999"));
    }

    // -- webhook url ---------------------------------------------------------

    #[test]
    fn accepts_ordinary_endpoints_including_local_ones() {
        for url in [
            "https://shop.acme.dev/webhooks",
            "http://localhost:4000/hooks",
            "https://acme.dev/hooks?env=test",
            "http://127.0.0.1:3000/",
        ] {
            assert!(webhook_url(url).is_ok(), "rejected {url}");
        }
    }

    #[test]
    fn normalises_what_it_returns() {
        // A bare authority gains the path the delivery worker will actually use.
        assert_eq!(webhook_url("https://acme.dev").unwrap(), "https://acme.dev/");
    }

    #[test]
    fn rejects_non_http_schemes() {
        for url in [
            "file:///etc/passwd",
            "gopher://acme.dev/",
            "ftp://acme.dev/hooks",
            "javascript:alert(1)",
        ] {
            let err = webhook_url(url).unwrap_err();
            assert_eq!(err.param.as_deref(), Some("url"), "accepted {url}");
        }
    }

    #[test]
    fn rejects_relative_and_empty_urls() {
        for url in ["", "/webhooks", "acme.dev/hooks", "://nope"] {
            assert!(webhook_url(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        let err = webhook_url("https://user:pass@acme.dev/hooks").unwrap_err();
        assert!(err.message.contains("credentials"));
    }

    #[test]
    fn rejects_fragments_and_oversized_urls() {
        assert!(webhook_url("https://acme.dev/hooks#section").is_err());
        let long = format!("https://acme.dev/{}", "a".repeat(MAX_URL_LEN));
        assert!(webhook_url(&long).is_err());
    }

    // -- enabled events ------------------------------------------------------

    #[test]
    fn an_empty_list_means_all_events() {
        assert!(enabled_events(&[]).is_ok());
    }

    #[test]
    fn accepts_well_shaped_types() {
        let events = ["payment.captured", "order.paid", "webhook_endpoint.created"]
            .map(String::from)
            .to_vec();
        assert!(enabled_events(&events).is_ok());
    }

    #[test]
    fn rejects_malformed_types() {
        for bad in ["payment", "Payment.Captured", "payment.", ".captured", "a b.c"] {
            let err = enabled_events(&[bad.to_string()]).unwrap_err();
            assert_eq!(err.param.as_deref(), Some("enabled_events"), "accepted {bad}");
        }
    }

    #[test]
    fn rejects_duplicates_and_oversized_lists() {
        let dup = vec!["payment.captured".to_string(); 2];
        assert!(enabled_events(&dup).unwrap_err().message.contains("Duplicate"));

        let many: Vec<String> = (0..=MAX_ENABLED_EVENTS)
            .map(|i| format!("payment.e{i}"))
            .collect();
        assert!(enabled_events(&many).is_err());
    }

    // -- idempotency key -----------------------------------------------------

    #[test]
    fn accepts_a_uuid_shaped_key() {
        let key = "0192f3a2-9c1e-7b3a-8f21-4c6d0e5a7b19";
        assert_eq!(idempotency_key(key).unwrap(), key);
    }

    #[test]
    fn rejects_keys_that_are_too_short_too_long_or_unprintable() {
        assert!(idempotency_key("short").is_err());
        assert!(idempotency_key(&"a".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1)).is_err());
        assert!(idempotency_key("key\nwith-newline").is_err());
        assert!(idempotency_key("key with space").is_err());
        assert!(idempotency_key("").is_err());
    }

    // -- helper --------------------------------------------------------------

    #[test]
    fn truncate_respects_character_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("héllo", 2), "hé…");
    }
}
