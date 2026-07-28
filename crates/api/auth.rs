//! Authentication middleware — the front door of the server API.
//!
//! Every authenticated route runs through [`require_auth`]. It turns an
//! `Authorization` header into an [`AuthCtx`] in the request extensions, or it
//! returns an error envelope. Nothing downstream is allowed to guess who the
//! caller is.
//!
//! # Credential model
//!
//! `Authorization: Bearer sk_test_...` — secret keys only. Publishable keys
//! (`pk_test_`) authenticate the checkout flow through a separate, far more
//! restricted path (v-checkout milestone) and are explicitly rejected here with
//! a message explaining why, because sending a publishable key to the server
//! API is the single most common integration mistake.
//!
//! Scheme matching is case-insensitive (RFC 7235 §2.1) and surrounding
//! whitespace is tolerated, because a merchant pasting a key out of a dashboard
//! should not spend an afternoon on a stray newline. Everything past that is
//! strict: exact prefix, hex body, bounded length.
//!
//! # Lookup
//!
//! The presented token is reduced to a deterministic SHA-256 (see
//! `crypto::api_key::lookup_hash` for why SHA-256 and not Argon2 for
//! high-entropy credentials) and matched against the unique index on
//! `api_keys.secret_hash`. The query filters `revoked_at IS NULL`, so a revoked
//! key does not resolve at all — there is no separate "is it active" check that
//! could be forgotten.
//!
//! # Caching, and what it costs
//!
//! An authenticated API does one database round trip per request purely to
//! answer "who is this", which at any real volume is the single most repeated
//! query in the system. [`AuthCache`] holds resolved (and rejected) lookups for
//! [`DEFAULT_CACHE_TTL`], keyed by the lookup hash — never by the token itself.
//!
//! The tradeoff is explicit: **revocation propagates within the TTL, not
//! instantly.** The revoke handler must call [`AuthCache::invalidate_key`] to
//! close that window in-process; a multi-node deployment additionally waits out
//! the TTL on its other nodes. Five seconds is chosen so that window stays
//! shorter than a human can act on it. Set the TTL to zero
//! ([`AuthCache::disabled`]) in tests that assert on revocation timing.
//!
//! Rejections are cached too. Without that, a credential-stuffing run turns
//! into one database query per guess, which is a free denial-of-service against
//! the pool.
//!
//! # Timing
//!
//! Lookups are bounded by [`LOOKUP_TIMEOUT`]. A database that is down or slow
//! must surface as a retryable 503, never as "Invalid API key" — telling a
//! merchant to rotate credentials during an outage is how a small incident
//! becomes a large one.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use domain::api_key::{ApiKeyKind, Scope};
use domain::id::{ApiKeyId, MerchantId};
use tracing::{debug, error, info_span, warn, Instrument};

use crate::error::ApiError;
use crate::state::AppState;

/// Prefix of a server-side secret key.
const SECRET_PREFIX: &str = "sk_test_";
/// Prefix of a browser-safe publishable key. Never valid on this API.
const PUBLISHABLE_PREFIX: &str = "pk_test_";

/// Longest `Authorization` header value we will even look at. A real key is
/// ~72 bytes; anything past this is noise, and hashing attacker-sized input is
/// work we should decline to do.
const MAX_HEADER_BYTES: usize = 512;

/// Accepted bounds for the hex body after the prefix. The generator currently
/// emits 64 chars (32 bytes); the range leaves room for the key format to grow
/// without this file becoming the thing that blocks it.
const MIN_BODY_LEN: usize = 32;
const MAX_BODY_LEN: usize = 128;

/// How long a resolved or rejected lookup stays cached. Also the worst-case
/// delay before a revocation takes effect on a node that did not process it.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5);

/// Upper bound on cached lookups. Sized so that a normal merchant population
/// fits comfortably and a stuffing run cannot grow the process heap.
pub const DEFAULT_CACHE_CAPACITY: usize = 4_096;

/// Minimum spacing between `last_used_at` writes for a single key. Without
/// this, every request issues an `UPDATE` against the same row — write
/// amplification and row-lock contention on the hot path, in exchange for a
/// timestamp nobody reads at second granularity.
pub const DEFAULT_TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Hard ceiling on the key lookup. Past this the request fails as unavailable
/// rather than hanging on a wedged pool.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Realm advertised in `WWW-Authenticate` challenges (RFC 6750 §3).
const CHALLENGE_ANONYMOUS: &str = r#"Bearer realm="payment-sandbox""#;
const CHALLENGE_INVALID_TOKEN: &str = r#"Bearer realm="payment-sandbox", error="invalid_token""#;
const CHALLENGE_INSUFFICIENT_SCOPE: &str =
    r#"Bearer realm="payment-sandbox", error="insufficient_scope""#;

// ---------------------------------------------------------------------------
// Scopes
// ---------------------------------------------------------------------------

/// Every scope that exists, in a stable order. Kept in sync with [`bit`] below:
/// adding a variant to `Scope` fails to compile there, which is the reminder to
/// update this array.
const ALL_SCOPES: [Scope; 6] = [
    Scope::OrdersRead,
    Scope::OrdersWrite,
    Scope::PaymentsRead,
    Scope::PaymentsWrite,
    Scope::RefundsWrite,
    Scope::WebhooksManage,
];

/// One bit per scope. The match is exhaustive on purpose: a new `Scope` variant
/// breaks this build instead of silently authorizing nothing.
const fn bit(scope: Scope) -> u16 {
    match scope {
        Scope::OrdersRead => 1 << 0,
        Scope::OrdersWrite => 1 << 1,
        Scope::PaymentsRead => 1 << 2,
        Scope::PaymentsWrite => 1 << 3,
        Scope::RefundsWrite => 1 << 4,
        Scope::WebhooksManage => 1 << 5,
    }
}

/// A key's grants as a bitset.
///
/// The database hands us a `Vec<Scope>`; carrying that into every request would
/// mean an allocation per clone of the auth context on the hot path. A `u16`
/// makes [`AuthCtx`] `Copy`, makes the permission check a single AND, and makes
/// it impossible to accidentally hold duplicate or ordered scopes.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeSet(u16);

impl ScopeSet {
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn from_slice(scopes: &[Scope]) -> Self {
        scopes.iter().copied().collect()
    }

    /// Every scope that exists. Convenience for tests and for the key-issuing
    /// path; not a default anyone gets implicitly.
    #[must_use]
    pub fn all() -> Self {
        Self::from_slice(&ALL_SCOPES)
    }

    #[must_use]
    pub const fn with(self, scope: Scope) -> Self {
        Self(self.0 | bit(scope))
    }

    #[must_use]
    pub const fn contains(self, scope: Scope) -> bool {
        self.0 & bit(scope) != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// Iterate the granted scopes in [`ALL_SCOPES`] order.
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        ALL_SCOPES.into_iter().filter(move |s| self.contains(*s))
    }
}

impl FromIterator<Scope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        iter.into_iter().fold(Self::empty(), Self::with)
    }
}

impl std::fmt::Debug for ScopeSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(self.iter().map(Scope::as_str))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Auth context
// ---------------------------------------------------------------------------

/// The authenticated caller, inserted into request extensions by the middleware
/// and read by handlers via `Extension<AuthCtx>`.
///
/// `Copy`, so passing it around costs nothing and no handler is tempted to hold
/// a reference to it across an await point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthCtx {
    pub merchant_id: MerchantId,
    pub key_id: ApiKeyId,
    pub scopes: ScopeSet,
}

impl AuthCtx {
    #[must_use]
    pub fn new(merchant_id: MerchantId, key_id: ApiKeyId, scopes: ScopeSet) -> Self {
        Self {
            merchant_id,
            key_id,
            scopes,
        }
    }

    /// Handlers call this first. A key without the scope gets a 403 that names
    /// the missing scope — actionable, without leaking anything else.
    pub fn require(&self, scope: Scope) -> Result<(), ApiError> {
        if self.scopes.contains(scope) {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!(
                "This API key does not have the '{}' scope.",
                scope.as_str()
            )))
        }
    }

    /// Require every listed scope. Names the first one missing rather than
    /// making the caller discover them one failed request at a time.
    pub fn require_all(&self, scopes: &[Scope]) -> Result<(), ApiError> {
        for scope in scopes {
            self.require(*scope)?;
        }
        Ok(())
    }

    /// Require at least one of the listed scopes — for routes reachable under
    /// more than one grant.
    pub fn require_any(&self, scopes: &[Scope]) -> Result<(), ApiError> {
        if scopes.iter().any(|s| self.scopes.contains(*s)) {
            return Ok(());
        }
        let names = scopes
            .iter()
            .map(|s| format!("'{}'", s.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        Err(ApiError::forbidden(format!(
            "This API key needs at least one of these scopes: {names}."
        )))
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// What a previous lookup of a given token hash concluded.
#[derive(Clone, Copy)]
enum Outcome {
    Resolved(AuthCtx),
    /// No active key matches. Cached so that a guessing run costs the attacker
    /// a request and costs us nothing.
    Rejected,
}

struct Entry {
    outcome: Outcome,
    expires_at: Instant,
}

/// Short-lived memo of API key lookups, plus the throttle for `last_used_at`
/// writes. Lives on [`AppState`]; one per process.
///
/// Expiry is measured with [`Instant`], not the injected `Clock`. The sandbox
/// deliberately lets tests jump the clock days forward (and back) to watch
/// settlement resolve; a security-relevant TTL must not be movable by an API
/// call, so it uses the monotonic clock instead.
pub struct AuthCache {
    ttl: Duration,
    capacity: usize,
    touch_interval: Duration,
    entries: RwLock<HashMap<String, Entry>>,
    touches: RwLock<HashMap<ApiKeyId, Instant>>,
}

impl Default for AuthCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthCache {
    #[must_use]
    pub fn new() -> Self {
        Self::with_settings(
            DEFAULT_CACHE_TTL,
            DEFAULT_CACHE_CAPACITY,
            DEFAULT_TOUCH_INTERVAL,
        )
    }

    /// A cache that never hits. Use in tests that assert a revoked key stops
    /// working on the very next request.
    #[must_use]
    pub fn disabled() -> Self {
        Self::with_settings(Duration::ZERO, 0, Duration::ZERO)
    }

    #[must_use]
    pub fn with_settings(ttl: Duration, capacity: usize, touch_interval: Duration) -> Self {
        Self {
            ttl,
            capacity,
            touch_interval,
            entries: RwLock::new(HashMap::new()),
            touches: RwLock::new(HashMap::new()),
        }
    }

    fn is_enabled(&self) -> bool {
        !self.ttl.is_zero() && self.capacity > 0
    }

    fn lookup(&self, hash: &str, now: Instant) -> Option<Outcome> {
        if !self.is_enabled() {
            return None;
        }
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(hash)?;
        // Expired entries are left for `store` to sweep; reading is the hot
        // path and must not take a write lock.
        (entry.expires_at > now).then_some(entry.outcome)
    }

    fn store(&self, hash: &str, outcome: Outcome, now: Instant) {
        if !self.is_enabled() {
            return;
        }
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);

        if entries.len() >= self.capacity {
            entries.retain(|_, e| e.expires_at > now);
            // Still full: every entry is live, which at this size means we are
            // under a stuffing run. Drop everything — a cache miss is correct,
            // just slower, and a bounded heap matters more.
            if entries.len() >= self.capacity {
                entries.clear();
            }
        }

        entries.insert(
            hash.to_owned(),
            Entry {
                outcome,
                expires_at: now + self.ttl,
            },
        );
    }

    /// Drop every cached decision for one key. The revoke handler MUST call
    /// this; without it a revoked key keeps working for up to the TTL.
    pub fn invalidate_key(&self, key_id: ApiKeyId) {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, e| !matches!(e.outcome, Outcome::Resolved(ctx) if ctx.key_id == key_id));
        drop(entries);

        let mut touches = self.touches.write().unwrap_or_else(PoisonError::into_inner);
        touches.remove(&key_id);
    }

    /// Drop everything. For scope changes applied merchant-wide, and for tests.
    pub fn invalidate_all(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.touches
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Number of cached decisions, live or expired. Diagnostics only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True at most once per [`DEFAULT_TOUCH_INTERVAL`] per key. Records the
    /// decision, so two concurrent requests cannot both win.
    fn should_touch(&self, key_id: ApiKeyId, now: Instant) -> bool {
        if self.touch_interval.is_zero() {
            return true;
        }
        let mut touches = self.touches.write().unwrap_or_else(PoisonError::into_inner);

        if let Some(last) = touches.get(&key_id) {
            if now.duration_since(*last) < self.touch_interval {
                return false;
            }
        }
        // Bounded like `entries`, for the same reason: this map is keyed by a
        // value an attacker cannot forge, but a large merchant population is
        // reason enough not to let it grow forever.
        if touches.len() >= self.capacity.max(1) {
            touches.retain(|_, last| now.duration_since(*last) < self.touch_interval);
        }
        touches.insert(key_id, now);
        true
    }
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Authenticate the request, or reject it.
///
/// Returns a `Response` rather than a `Result` so that every path — including
/// errors raised by downstream handlers — passes through
/// [`with_challenge`] and carries the `WWW-Authenticate` header RFC 7235
/// requires on a 401.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    // Captured before `req` is consumed: RFC 6750 §3 wants a bare challenge
    // when no credentials were offered, and an `error="invalid_token"` one when
    // they were offered and were bad.
    let had_credentials = req.headers().contains_key(AUTHORIZATION);

    let response = match authenticate(&state, req, next).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    };

    with_challenge(response, had_credentials)
}

async fn authenticate(
    state: &Arc<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_bearer(req.headers())?;
    validate_shape(token)?;

    // From here on the plaintext is never logged, stored, or compared — only
    // its lookup hash travels.
    let hash = crypto::api_key::lookup_hash(token);
    let ctx = resolve(state, &hash).await?;

    req.extensions_mut().insert(ctx);

    // Every log line emitted downstream is now attributable to a merchant and a
    // key without each handler having to remember to say so.
    let span = info_span!(
        "authenticated",
        merchant_id = %ctx.merchant_id,
        api_key_id = %ctx.key_id,
    );
    Ok(next.run(req).instrument(span).await)
}

/// Pull the token out of `Authorization`, with errors that name the actual
/// mistake. Scheme comparison is case-insensitive; the token is trimmed.
fn extract_bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();

    let raw = values.next().ok_or_else(|| {
        ApiError::unauthorized("Missing API key. Send 'Authorization: Bearer sk_test_...'.")
    })?;

    // Two `Authorization` headers is never a legitimate client; treating it as
    // "use the first" is exactly the ambiguity request smuggling relies on.
    if values.next().is_some() {
        return Err(ApiError::unauthorized(
            "Multiple Authorization headers were sent. Send exactly one.",
        ));
    }

    if raw.len() > MAX_HEADER_BYTES {
        return Err(ApiError::unauthorized("Authorization header is too long."));
    }

    let raw = raw.to_str().map_err(|_| {
        ApiError::unauthorized("Authorization header must contain only ASCII characters.")
    })?;

    let (scheme, rest) = raw.trim().split_once(' ').ok_or_else(|| {
        ApiError::unauthorized(
            "Malformed Authorization header. Expected 'Authorization: Bearer sk_test_...'.",
        )
    })?;

    if !scheme.eq_ignore_ascii_case("Bearer") {
        // Echo the scheme only when it is plainly a scheme name, so an attacker
        // cannot use the error envelope as a reflector for arbitrary text.
        let named = scheme.len() <= 16 && scheme.chars().all(|c| c.is_ascii_alphanumeric());
        return Err(ApiError::unauthorized(if named {
            format!("Unsupported authorization scheme '{scheme}'. Use 'Bearer'.")
        } else {
            "Unsupported authorization scheme. Use 'Bearer'.".to_string()
        }));
    }

    Ok(rest.trim())
}

/// Reject anything that cannot possibly be an active secret key before it costs
/// us a hash and a query. The publishable-key case gets its own message because
/// it is the mistake integrators actually make.
fn validate_shape(token: &str) -> Result<(), ApiError> {
    if token.is_empty() {
        return Err(ApiError::unauthorized(
            "Missing API key. Send 'Authorization: Bearer sk_test_...'.",
        ));
    }

    if token.starts_with(PUBLISHABLE_PREFIX) {
        return Err(ApiError::unauthorized(
            "Publishable keys cannot call the server API. Use your secret key (sk_test_...) — and never expose it in browser code.",
        ));
    }

    if !crypto::api_key::is_test_key(token) {
        // A live-shaped credential in a sandbox is worth calling out loudly: it
        // means someone is one config mistake away from a real incident.
        if token.starts_with("sk_live_") || token.starts_with("pk_live_") {
            warn!("rejected a live-shaped credential presented to the sandbox");
            return Err(ApiError::unauthorized(
                "This sandbox only accepts test keys (sk_test_...). Never send a live key here.",
            ));
        }
        return Err(ApiError::unauthorized("Invalid API key format."));
    }

    let body = &token[SECRET_PREFIX.len()..];
    let well_formed = (MIN_BODY_LEN..=MAX_BODY_LEN).contains(&body.len())
        && body.bytes().all(|b| b.is_ascii_hexdigit());

    if !well_formed {
        return Err(ApiError::unauthorized("Invalid API key format."));
    }

    Ok(())
}

/// Cache, then database. Distinguishes "no such key" (401) from "we could not
/// find out" (503) — collapsing those two is the classic auth-middleware bug.
async fn resolve(state: &Arc<AppState>, hash: &str) -> Result<AuthCtx, ApiError> {
    let now = Instant::now();

    if let Some(outcome) = state.auth_cache.lookup(hash, now) {
        return match outcome {
            Outcome::Resolved(ctx) => Ok(ctx),
            Outcome::Rejected => Err(invalid_key()),
        };
    }

    let lookup = store::api_key::find_active_by_hash(&state.pool, hash);
    let key = match tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await {
        Ok(Ok(key)) => key,

        Ok(Err(err)) if err.is_not_found() => {
            state.auth_cache.store(hash, Outcome::Rejected, now);
            warn!(key_fingerprint = %fingerprint(hash), "rejected unknown or revoked api key");
            return Err(invalid_key());
        }

        // The database answered with something other than "no such row", or did
        // not answer at all. Not the caller's fault; do not cache, do not tell
        // them their key is bad.
        Ok(Err(err)) => {
            error!(error = ?err, "api key lookup failed");
            return Err(ApiError::unavailable());
        }
        Err(_elapsed) => {
            error!(timeout = ?LOOKUP_TIMEOUT, "api key lookup timed out");
            return Err(ApiError::unavailable());
        }
    };

    // Defense in depth. The `pk_test_` prefix check above already makes this
    // unreachable, so reaching it means the two representations of "kind" have
    // drifted apart — a bug worth an error-level line.
    if key.kind != ApiKeyKind::Secret {
        error!(
            key_id = %key.id,
            "a non-secret key resolved from a secret-shaped token; refusing it"
        );
        state.auth_cache.store(hash, Outcome::Rejected, now);
        return Err(invalid_key());
    }

    let ctx = AuthCtx::new(key.merchant_id, key.id, ScopeSet::from_slice(&key.scopes));
    state.auth_cache.store(hash, Outcome::Resolved(ctx), now);

    if ctx.scopes.is_empty() {
        // Valid credential, zero grants. Authentication succeeds and every
        // handler will 403; say so once here so the cause is obvious in logs.
        debug!(key_id = %ctx.key_id, "authenticated a key that holds no scopes");
    }

    schedule_touch(state, ctx.key_id, now);

    Ok(ctx)
}

/// Record that the key was used. Best-effort and throttled: it must never fail
/// the request, slow it down, or write more often than anyone reads.
fn schedule_touch(state: &Arc<AppState>, key_id: ApiKeyId, now: Instant) {
    if !state.auth_cache.should_touch(key_id, now) {
        return;
    }

    let pool = state.pool.clone();
    let at = state.clock.now();
    tokio::spawn(async move {
        if let Err(err) = store::api_key::touch_last_used(&pool, key_id, at).await {
            debug!(error = ?err, %key_id, "failed to record api key last_used_at");
        }
    });
}

/// The single response to "this credential does not work". Deliberately
/// identical whether the key never existed, was revoked, or belongs to another
/// environment — the distinction is an oracle, and the merchant's action is the
/// same in all three cases.
fn invalid_key() -> ApiError {
    ApiError::unauthorized("Invalid API key.")
}

/// A short, non-reversible tag for correlating log lines about one credential.
/// Derived from the SHA-256 lookup hash, never from the token, and truncated so
/// that logs cannot be replayed against the `secret_hash` index.
fn fingerprint(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Attach the RFC 7235 / RFC 6750 challenge to any 401 or 403 leaving this
/// stack, including ones produced by handlers via `AuthCtx::require`. An
/// existing header always wins.
fn with_challenge(mut response: Response, had_credentials: bool) -> Response {
    let challenge = match response.status() {
        StatusCode::UNAUTHORIZED if had_credentials => CHALLENGE_INVALID_TOKEN,
        StatusCode::UNAUTHORIZED => CHALLENGE_ANONYMOUS,
        StatusCode::FORBIDDEN => CHALLENGE_INSUFFICIENT_SCOPE,
        _ => return response,
    };

    let headers = response.headers_mut();
    if !headers.contains_key(WWW_AUTHENTICATE) {
        headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn valid_token() -> String {
        format!("{SECRET_PREFIX}{}", "a1b2c3d4".repeat(8)) // 64 hex chars
    }

    fn ctx(scopes: ScopeSet) -> AuthCtx {
        AuthCtx::new(MerchantId::new(), ApiKeyId::new(), scopes)
    }

    // -- scope set ---------------------------------------------------------

    #[test]
    fn all_scopes_array_covers_every_variant() {
        // If a variant is added to `Scope`, `bit` stops compiling and this
        // catches the case where `ALL_SCOPES` was not updated alongside it.
        assert_eq!(ALL_SCOPES.len(), Scope::full_access().len());
        assert_eq!(ScopeSet::all().len() as usize, ALL_SCOPES.len());
    }

    #[test]
    fn every_scope_gets_a_distinct_bit() {
        let mut seen = 0u16;
        for scope in ALL_SCOPES {
            let b = bit(scope);
            assert_eq!(seen & b, 0, "duplicate bit for {}", scope.as_str());
            seen |= b;
        }
    }

    #[test]
    fn scope_set_roundtrips_through_a_slice() {
        let set = ScopeSet::from_slice(&[Scope::PaymentsRead, Scope::RefundsWrite]);
        assert!(set.contains(Scope::PaymentsRead));
        assert!(set.contains(Scope::RefundsWrite));
        assert!(!set.contains(Scope::PaymentsWrite));
        assert_eq!(set.len(), 2);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            vec![Scope::PaymentsRead, Scope::RefundsWrite]
        );
    }

    #[test]
    fn duplicate_scopes_collapse() {
        let set = ScopeSet::from_slice(&[Scope::OrdersRead, Scope::OrdersRead]);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn empty_scope_set_grants_nothing() {
        let set = ScopeSet::empty();
        assert!(set.is_empty());
        for scope in ALL_SCOPES {
            assert!(!set.contains(scope));
        }
    }

    // -- require -----------------------------------------------------------

    #[test]
    fn require_names_the_missing_scope() {
        let c = ctx(ScopeSet::from_slice(&[Scope::OrdersRead]));
        assert!(c.require(Scope::OrdersRead).is_ok());

        let err = c.require(Scope::RefundsWrite).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.code, "insufficient_scope");
        assert!(err.message.contains("refunds:write"));
    }

    #[test]
    fn require_all_fails_on_the_first_missing_scope() {
        let c = ctx(ScopeSet::from_slice(&[Scope::OrdersRead]));
        assert!(c.require_all(&[Scope::OrdersRead]).is_ok());

        let err = c
            .require_all(&[Scope::OrdersRead, Scope::OrdersWrite])
            .unwrap_err();
        assert!(err.message.contains("orders:write"));
    }

    #[test]
    fn require_any_accepts_a_single_match_and_lists_alternatives() {
        let c = ctx(ScopeSet::from_slice(&[Scope::PaymentsRead]));
        assert!(c
            .require_any(&[Scope::PaymentsWrite, Scope::PaymentsRead])
            .is_ok());

        let err = c
            .require_any(&[Scope::RefundsWrite, Scope::WebhooksManage])
            .unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert!(err.message.contains("refunds:write"));
        assert!(err.message.contains("webhooks:manage"));
    }

    // -- header parsing ----------------------------------------------------

    #[test]
    fn extracts_a_well_formed_bearer_token() {
        let token = valid_token();
        let headers = header(&format!("Bearer {token}"));
        assert_eq!(extract_bearer(&headers).unwrap(), token);
    }

    #[test]
    fn scheme_is_case_insensitive() {
        let token = valid_token();
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let headers = header(&format!("{scheme} {token}"));
            assert_eq!(extract_bearer(&headers).unwrap(), token);
        }
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let token = valid_token();
        let headers = header(&format!("  Bearer   {token}   "));
        assert_eq!(extract_bearer(&headers).unwrap(), token);
    }

    #[test]
    fn missing_header_is_unauthorized() {
        let err = extract_bearer(&HeaderMap::new()).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert!(err.message.contains("Missing API key"));
    }

    #[test]
    fn duplicate_headers_are_rejected_rather_than_resolved() {
        let mut headers = header(&format!("Bearer {}", valid_token()));
        headers.append(AUTHORIZATION, HeaderValue::from_static("Bearer sk_test_x"));

        let err = extract_bearer(&headers).unwrap_err();
        assert!(err.message.contains("Multiple Authorization headers"));
    }

    #[test]
    fn oversized_header_is_rejected_before_parsing() {
        let headers = header(&format!("Bearer sk_test_{}", "a".repeat(MAX_HEADER_BYTES)));
        let err = extract_bearer(&headers).unwrap_err();
        assert!(err.message.contains("too long"));
    }

    #[test]
    fn header_without_a_space_is_malformed() {
        let headers = header(&valid_token());
        let err = extract_bearer(&headers).unwrap_err();
        assert!(err.message.contains("Malformed"));
    }

    #[test]
    fn other_schemes_are_named_when_safe_to_echo() {
        let headers = header("Basic dXNlcjpwYXNz");
        let err = extract_bearer(&headers).unwrap_err();
        assert!(err.message.contains("'Basic'"), "{}", err.message);
    }

    #[test]
    fn a_hostile_scheme_is_not_reflected() {
        let headers = header("<script>alert(1)</script> token");
        let err = extract_bearer(&headers).unwrap_err();
        assert!(!err.message.contains("script"), "{}", err.message);
    }

    // -- shape validation --------------------------------------------------

    #[test]
    fn accepts_a_correctly_shaped_secret_key() {
        assert!(validate_shape(&valid_token()).is_ok());
    }

    #[test]
    fn publishable_keys_get_their_own_explanation() {
        let err = validate_shape(&format!("{PUBLISHABLE_PREFIX}{}", "ab".repeat(32))).unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert!(err.message.contains("Publishable keys"));
        assert!(err.message.contains("sk_test_"));
    }

    #[test]
    fn live_shaped_keys_are_called_out() {
        let err = validate_shape(&format!("sk_live_{}", "ab".repeat(32))).unwrap_err();
        assert!(err.message.contains("only accepts test keys"));
    }

    #[test]
    fn malformed_bodies_are_rejected_without_a_query() {
        for token in [
            "",
            "sk_test_",
            "sk_test_short",
            "sk_test_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", // non-hex
            "nonsense",
            "Bearer sk_test_abc",
        ] {
            assert!(
                validate_shape(token).is_err(),
                "expected rejection for {token:?}"
            );
        }
    }

    #[test]
    fn body_length_bounds_are_enforced_at_both_ends() {
        let just_under = format!("{SECRET_PREFIX}{}", "a".repeat(MIN_BODY_LEN - 1));
        let just_over = format!("{SECRET_PREFIX}{}", "a".repeat(MAX_BODY_LEN + 1));
        let at_min = format!("{SECRET_PREFIX}{}", "a".repeat(MIN_BODY_LEN));
        let at_max = format!("{SECRET_PREFIX}{}", "a".repeat(MAX_BODY_LEN));

        assert!(validate_shape(&just_under).is_err());
        assert!(validate_shape(&just_over).is_err());
        assert!(validate_shape(&at_min).is_ok());
        assert!(validate_shape(&at_max).is_ok());
    }

    #[test]
    fn a_real_generated_key_passes_validation() {
        // Guards against this file's shape rules drifting from the generator.
        let key = crypto::api_key::generate("sk_test").unwrap();
        assert!(validate_shape(&key.plaintext).is_ok());
    }

    // -- cache -------------------------------------------------------------

    #[test]
    fn cache_returns_a_stored_decision_within_the_ttl() {
        let cache = AuthCache::new();
        let now = Instant::now();
        let c = ctx(ScopeSet::all());

        assert!(cache.lookup("h", now).is_none());
        cache.store("h", Outcome::Resolved(c), now);

        let hit = cache.lookup("h", now + Duration::from_secs(1)).unwrap();
        assert!(matches!(hit, Outcome::Resolved(got) if got == c));
    }

    #[test]
    fn cache_expires_on_the_monotonic_clock() {
        let cache = AuthCache::new();
        let now = Instant::now();
        cache.store("h", Outcome::Resolved(ctx(ScopeSet::all())), now);

        assert!(cache.lookup("h", now + DEFAULT_CACHE_TTL).is_none());
    }

    #[test]
    fn rejections_are_cached_too() {
        let cache = AuthCache::new();
        let now = Instant::now();
        cache.store("bad", Outcome::Rejected, now);

        assert!(matches!(
            cache.lookup("bad", now).unwrap(),
            Outcome::Rejected
        ));
    }

    #[test]
    fn disabled_cache_never_hits() {
        let cache = AuthCache::disabled();
        let now = Instant::now();
        cache.store("h", Outcome::Resolved(ctx(ScopeSet::all())), now);

        assert!(cache.lookup("h", now).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn invalidate_key_drops_only_that_keys_entries() {
        let cache = AuthCache::new();
        let now = Instant::now();
        let mine = ctx(ScopeSet::all());
        let theirs = ctx(ScopeSet::all());

        cache.store("mine", Outcome::Resolved(mine), now);
        cache.store("theirs", Outcome::Resolved(theirs), now);
        cache.store("rejected", Outcome::Rejected, now);

        cache.invalidate_key(mine.key_id);

        assert!(cache.lookup("mine", now).is_none());
        assert!(cache.lookup("theirs", now).is_some());
        assert!(cache.lookup("rejected", now).is_some());
    }

    #[test]
    fn invalidate_all_empties_the_cache() {
        let cache = AuthCache::new();
        let now = Instant::now();
        cache.store("a", Outcome::Resolved(ctx(ScopeSet::all())), now);
        cache.store("b", Outcome::Rejected, now);

        cache.invalidate_all();
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_stays_within_its_capacity_under_a_flood() {
        let cache = AuthCache::with_settings(DEFAULT_CACHE_TTL, 8, DEFAULT_TOUCH_INTERVAL);
        let now = Instant::now();

        for i in 0..1_000 {
            cache.store(&format!("hash-{i}"), Outcome::Rejected, now);
        }
        assert!(cache.len() <= 8, "cache grew to {}", cache.len());
    }

    #[test]
    fn expired_entries_are_swept_before_the_cache_is_cleared() {
        let cache = AuthCache::with_settings(Duration::from_secs(5), 2, DEFAULT_TOUCH_INTERVAL);
        let start = Instant::now();
        cache.store("old-a", Outcome::Rejected, start);
        cache.store("old-b", Outcome::Rejected, start);

        // Both are expired by now, so the sweep makes room without discarding
        // anything live.
        let later = start + Duration::from_secs(10);
        let live = ctx(ScopeSet::all());
        cache.store("fresh", Outcome::Resolved(live), later);

        assert!(cache.lookup("old-a", later).is_none());
        assert!(cache.lookup("fresh", later).is_some());
    }

    // -- last_used_at throttle ---------------------------------------------

    #[test]
    fn touch_is_throttled_per_key() {
        let cache = AuthCache::new();
        let key = ApiKeyId::new();
        let now = Instant::now();

        assert!(cache.should_touch(key, now));
        assert!(!cache.should_touch(key, now + Duration::from_secs(1)));
        assert!(!cache.should_touch(key, now + DEFAULT_TOUCH_INTERVAL - Duration::from_millis(1)));
        assert!(cache.should_touch(key, now + DEFAULT_TOUCH_INTERVAL));
    }

    #[test]
    fn touch_throttle_is_independent_across_keys() {
        let cache = AuthCache::new();
        let now = Instant::now();
        let a = ApiKeyId::new();
        let b = ApiKeyId::new();

        assert!(cache.should_touch(a, now));
        assert!(cache.should_touch(b, now));
        assert!(!cache.should_touch(a, now));
    }

    #[test]
    fn revoking_a_key_also_clears_its_touch_throttle() {
        let cache = AuthCache::new();
        let key = ApiKeyId::new();
        let now = Instant::now();

        assert!(cache.should_touch(key, now));
        cache.invalidate_key(key);
        assert!(cache.should_touch(key, now));
    }

    // -- challenge header --------------------------------------------------

    #[test]
    fn unauthenticated_401_advertises_a_bare_challenge() {
        let res = with_challenge(invalid_key().into_response(), false);
        assert_eq!(
            res.headers().get(WWW_AUTHENTICATE).unwrap(),
            CHALLENGE_ANONYMOUS
        );
    }

    #[test]
    fn a_bad_credential_401_advertises_invalid_token() {
        let res = with_challenge(invalid_key().into_response(), true);
        assert_eq!(
            res.headers().get(WWW_AUTHENTICATE).unwrap(),
            CHALLENGE_INVALID_TOKEN
        );
    }

    #[test]
    fn a_403_advertises_insufficient_scope() {
        let err = ctx(ScopeSet::empty())
            .require(Scope::PaymentsWrite)
            .unwrap_err();
        let res = with_challenge(err.into_response(), true);
        assert_eq!(
            res.headers().get(WWW_AUTHENTICATE).unwrap(),
            CHALLENGE_INSUFFICIENT_SCOPE
        );
    }

    #[test]
    fn successful_responses_are_left_alone() {
        let res = with_challenge(StatusCode::OK.into_response(), true);
        assert!(res.headers().get(WWW_AUTHENTICATE).is_none());
    }

    #[test]
    fn an_existing_challenge_is_not_overwritten() {
        let mut res = invalid_key().into_response();
        res.headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Custom"));

        let res = with_challenge(res, true);
        assert_eq!(res.headers().get(WWW_AUTHENTICATE).unwrap(), "Custom");
    }

    // -- log hygiene -------------------------------------------------------

    #[test]
    fn fingerprint_is_short_and_never_the_whole_hash() {
        let hash = crypto::api_key::lookup_hash(&valid_token());
        let fp = fingerprint(&hash);
        assert_eq!(fp.len(), 12);
        assert!(hash.starts_with(fp));
        assert!(fp.len() < hash.len());
    }

    #[test]
    fn fingerprint_does_not_panic_on_a_short_input() {
        assert_eq!(fingerprint("abc"), "abc");
        assert_eq!(fingerprint(""), "");
    }
}
