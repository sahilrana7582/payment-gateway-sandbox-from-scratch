//! Cursor pagination, defined once for every collection endpoint.
//!
//! Two decisions are worth stating, because both are the opposite of what a
//! quick implementation does.
//!
//! **Out-of-range limits are an error, not a clamp.** A caller asking for
//! `limit=500` and silently receiving 100 items has no way to distinguish that
//! from a collection with 100 items in it — so they stop paginating and lose the
//! rest of their data. Rejecting with a message that names the maximum costs one
//! round trip during integration and zero afterwards.
//!
//! **The cursor is a timestamp, not an offset.** `OFFSET 400` re-reads the first
//! 400 rows on every page and, worse, shifts under concurrent inserts: a
//! merchant taking payments while paginating will see duplicates and gaps. A
//! `created_at < cursor` cursor is stable under insert and uses the index.
//!
//! `has_more` is computed by fetching one row past the page rather than issuing
//! a `COUNT(*)` — the count would be a second full scan to answer a boolean.
//!
//! # Known limitation
//!
//! The cursor is `created_at` alone, so rows sharing a timestamp to the
//! microsecond can be skipped at a page boundary: the next page asks for
//! `created_at < cursor`, which excludes the ties that were left behind. Real
//! traffic collides rarely at that resolution, and tests that use a fixed clock
//! collide always. The fix is a composite `(created_at, id)` cursor, which is a
//! change to the store's `WHERE` clause and its index, not to this module —
//! left deliberately for when list endpoints carry real volume.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::ApiConfig;
use crate::error::{ApiError, ApiResult};

/// Raw query parameters, exactly as they arrive.
///
/// `deny_unknown_fields` catches `?limitt=5`, which would otherwise be ignored
/// and hand back a default-sized page that looks like a working integration.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub limit: Option<i64>,
    /// RFC3339 timestamp cursor: return items created strictly before this.
    pub before: Option<String>,
}

impl ListParams {
    /// Validate against the configured bounds, producing the resolved request.
    pub fn resolve(&self, config: &ApiConfig) -> ApiResult<PageRequest> {
        let limit = match self.limit {
            None => config.default_page_size,
            Some(n) if (1..=config.max_page_size).contains(&n) => n,
            Some(n) => {
                return Err(ApiError::invalid_request(
                    "limit_invalid",
                    format!(
                        "'limit' must be between 1 and {}, got {n}.",
                        config.max_page_size
                    ),
                    Some("limit"),
                ))
            }
        };

        let before = self
            .before
            .as_deref()
            .map(|raw| {
                OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| {
                    ApiError::invalid_request(
                        "cursor_invalid",
                        "'before' must be an RFC3339 timestamp, as returned in 'next_before'.",
                        Some("before"),
                    )
                })
            })
            .transpose()?;

        Ok(PageRequest { limit, before })
    }
}

/// A validated page request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    pub limit: i64,
    pub before: Option<OffsetDateTime>,
}

impl PageRequest {
    /// How many rows to actually ask the store for: one more than the page, so
    /// the extra row's existence answers `has_more`.
    #[must_use]
    pub fn fetch_limit(&self) -> i64 {
        self.limit.saturating_add(1)
    }

    /// Trim an over-fetched result set into a page.
    ///
    /// `render` shapes each item for the wire and `cursor` reads the field the
    /// next page will seek from — passing both keeps this generic over every
    /// collection without the module needing to know about orders or events.
    pub fn finish<T>(
        &self,
        mut items: Vec<T>,
        render: impl Fn(&T) -> Value,
        cursor: impl Fn(&T) -> OffsetDateTime,
    ) -> Page {
        // `limit` is validated into 1..=max_page_size, so this cannot saturate
        // into a meaningful truncation on any supported platform.
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let has_more = items.len() > limit;
        items.truncate(limit);

        let next_before = has_more
            .then(|| items.last().map(&cursor))
            .flatten()
            .and_then(|t| t.format(&Rfc3339).ok());

        Page {
            data: items.iter().map(render).collect(),
            has_more,
            next_before,
        }
    }
}

/// A page of results, on the wire.
///
/// `object: "list"` lets a client dispatch on shape without consulting the URL
/// it called, which matters for generic SDK deserialisation.
#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub data: Vec<Value>,
    pub has_more: bool,
    /// Feed straight back as `?before=` to get the next page. `null` on the
    /// last page, so "keep going while this is non-null" is the whole client
    /// loop.
    pub next_before: Option<String>,
}

impl Page {
    #[must_use]
    pub fn into_json(self) -> Value {
        json!({
            "object": "list",
            "data": self.data,
            "has_more": self.has_more,
            "next_before": self.next_before,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn params(limit: Option<i64>, before: Option<&str>) -> ListParams {
        ListParams {
            limit,
            before: before.map(str::to_owned),
        }
    }

    #[derive(Debug)]
    struct Item(OffsetDateTime);

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item(datetime!(2026-01-01 00:00:00 UTC) + time::Duration::seconds(i as i64)))
            .collect()
    }

    fn render(i: &Item) -> Value {
        json!({ "at": i.0.format(&Rfc3339).unwrap() })
    }

    #[test]
    fn absent_limit_uses_the_configured_default() {
        let config = ApiConfig::default();
        let page = params(None, None).resolve(&config).unwrap();
        assert_eq!(page.limit, config.default_page_size);
        assert_eq!(page.before, None);
    }

    #[test]
    fn out_of_range_limits_are_rejected_with_the_maximum_named() {
        let config = ApiConfig::default();
        for bad in [0, -1, config.max_page_size + 1] {
            let err = params(Some(bad), None).resolve(&config).unwrap_err();
            assert_eq!(err.param.as_deref(), Some("limit"));
            assert!(
                err.message.contains(&config.max_page_size.to_string()),
                "message should name the max: {}",
                err.message
            );
        }
    }

    #[test]
    fn boundary_limits_are_accepted() {
        let config = ApiConfig::default();
        assert_eq!(params(Some(1), None).resolve(&config).unwrap().limit, 1);
        assert_eq!(
            params(Some(config.max_page_size), None)
                .resolve(&config)
                .unwrap()
                .limit,
            config.max_page_size
        );
    }

    #[test]
    fn a_malformed_cursor_names_itself() {
        let err = params(None, Some("last tuesday"))
            .resolve(&ApiConfig::default())
            .unwrap_err();
        assert_eq!(err.code, "cursor_invalid");
        assert_eq!(err.param.as_deref(), Some("before"));
    }

    #[test]
    fn a_valid_cursor_round_trips() {
        let resolved = params(None, Some("2026-01-01T00:00:00Z"))
            .resolve(&ApiConfig::default())
            .unwrap();
        assert_eq!(resolved.before, Some(datetime!(2026-01-01 00:00:00 UTC)));
    }

    #[test]
    fn a_full_page_reports_more_and_hands_back_a_usable_cursor() {
        let req = PageRequest {
            limit: 2,
            before: None,
        };
        assert_eq!(req.fetch_limit(), 3);

        // Store returned the over-fetch: three rows for a page of two.
        let page = req.finish(items(3), render, |i| i.0);
        assert_eq!(page.data.len(), 2);
        assert!(page.has_more);

        // The cursor is the LAST returned item, so the next page continues from
        // exactly where this one stopped — no gap, no repeat.
        let cursor = page.next_before.unwrap();
        assert_eq!(cursor, "2026-01-01T00:00:01Z");
    }

    #[test]
    fn a_final_page_reports_no_more_and_no_cursor() {
        let req = PageRequest {
            limit: 10,
            before: None,
        };
        let page = req.finish(items(3), render, |i| i.0);
        assert_eq!(page.data.len(), 3);
        assert!(!page.has_more);
        assert!(page.next_before.is_none());
    }

    #[test]
    fn an_empty_collection_is_a_page_not_an_error() {
        let req = PageRequest {
            limit: 10,
            before: None,
        };
        let page = req.finish(items(0), render, |i| i.0);
        assert!(page.data.is_empty());
        assert!(!page.has_more);
        assert!(page.next_before.is_none());
    }

    #[test]
    fn the_wire_shape_is_stable() {
        let req = PageRequest {
            limit: 1,
            before: None,
        };
        let json = req.finish(items(1), render, |i| i.0).into_json();
        assert_eq!(json["object"], "list");
        assert!(json["data"].is_array());
        assert_eq!(json["has_more"], false);
        assert!(json["next_before"].is_null());
    }
}
