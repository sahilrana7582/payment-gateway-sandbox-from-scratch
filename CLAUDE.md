# payment-sandbox — Agent Context

This file is the single source of truth for anyone (human or agent) writing code in
this repository. Read it fully before generating code. It describes **what the
project is**, **how it is currently structured after the API refactor**, **the
contracts that must not be broken**, and **the house style all new code must
match**.

---

## 1. What this project is

A **payment gateway sandbox** written in Rust — a Razorpay/Stripe-shaped *test*
gateway. Merchants call it with published test card numbers and get realistic
gateway behaviour: authorizations, captures, declines, disputes, a double-entry
ledger, a transactional outbox, and signed webhooks.

It is a sandbox, so no money moves. It is **not** a toy, so it is built to real
gateway standards: PCI-shaped handling of cardholder data, idempotency keys,
per-merchant isolation, a stable public error envelope, and a ledger that
balances.

**Design north star:** a merchant should be able to integrate against this, then
swap the base URL for a real gateway and change nothing else.

---

## 2. Hard constraints (never violate these)

| # | Constraint |
|---|------------|
| 1 | **Postgres runs on port `54432`, never 5432.** `DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev`. Every `psql`, `pg_isready`, `sqlx` or compose command must use 54432. |
| 2 | **A PAN (card number) never reaches a log line.** Not via `tracing`, not via `Debug`, not via a panic message. Types holding a PAN implement `Debug` by hand and redact it. |
| 3 | **A CVC is never stored.** It is shape-checked at the edge and dropped. No struct in `engine` or `store` has a field for it. |
| 4 | **A card number that is not in the published test set never touches the database.** The simulator gate runs before any write. |
| 5 | **Webhook signing secrets are shown exactly once**, in the creation response. Every other serialisation path goes through a `render()` that cannot emit it. |
| 6 | **Internal error detail never reaches the wire.** Store/ledger/engine internals map to a generic 500; the detail goes to logs. |
| 7 | **Every response body that is an error uses the one envelope** (§6). No bare strings, no axum default rejections. |
| 8 | **Cross-merchant access is a 404, never a 403.** Ownership failures must be indistinguishable from "does not exist" so ids cannot be probed. |
| 9 | **No `unwrap`/`expect`/`todo!` in non-test code.** CI denies them. |
| 10 | **`unsafe` is forbidden** — every crate has `#![forbid(unsafe_code)]`. |

---

## 3. Workspace layout

Rust 2021, resolver 2, 9 crates under `crates/`:

```
domain    → pure types + invariants. No I/O, no sqlx, no axum.
store     → persistence. sqlx queries, row structs, StoreError.
crypto    → hashing, peppered fingerprints, HMAC signing, AES-GCM, key generation.
queue     → job queue primitives over the `jobs` table.
simulator → test-card → outcome decision table. Pure, no I/O.
engine    → orchestration. Owns transactions, business rules, the outbox, the ledger writes.
webhooks  → delivery worker: signs, POSTs, retries.
api       → the HTTP surface. axum router, middleware, extractors, error envelope.
sandbox-server → the binary. Config from env, wiring, graceful shutdown.
```

### Dependency direction (strictly one-way)

```
domain ──▶ store ──▶ engine ──▶ api ──▶ sandbox-server
   │         │         ▲         ▲
crypto ──────┘         │         │
simulator ─────────────┘ (via api only)
queue ──▶ webhooks ────────────────┘
```

Rules:

- `domain` depends on nothing in the workspace.
- `engine` **does not depend on `simulator`.** The simulator's vocabulary is
  translated into the engine's vocabulary in `crates/api/routes/payments.rs`
  (`map_decision`). Keep it that way — it keeps the graph acyclic and lets every
  engine outcome be tested without a simulator.
- **`anyhow` is only allowed in `sandbox-server`.** Library crates use
  `thiserror` and typed errors.

### Pinned dependencies

axum 0.8.9 · tower 0.5 · tower-http 0.7.0 · sqlx 0.9.0 (postgres, offline via
`.sqlx/`) · tokio 1 · tracing 0.1 · time 0.3 · uuid (v7) · thiserror 1 ·
envconfig 0.11 · url 2 · reqwest 0.12 (rustls) · argon2 · hmac · sha2 · aes-gcm ·
rand · hex · serde/serde_json.

Note axum **0.8** path syntax: `"/orders/{id}"`, **not** `"/orders/:id"`.

---

## 4. The `api` crate (this is what the refactor changed)

### 4.1 What changed and why

Before: two files, `crates/api/router.rs` and `crates/api/handlers.rs`, written
fast to unblock Postman testing. Both are **deleted**. There was no request
correlation, no body limit, no request timeout, no CORS, no panic capture, no
idempotency enforcement (the `store::idempotency` module existed and was unused),
axum's default plain-text rejections leaked through, and `/events` returned a
bare array with no `has_more`.

After: a module-per-concern layout where each file states in its own doc comment
what it defends against.

### 4.2 File layout

> **`crates/api/Cargo.toml` sets `[lib] path = "lib.rs"` — the api crate's sources
> live at the crate root, not under `src/`.** Every other crate uses `src/`.

```
crates/api/
  lib.rs                  crate root: module table, re-exports, crate-level lints
  config.rs               ApiConfig — every operational limit, in one place
  state.rs                AppState — pool, clock, services, auth cache, config
  error.rs                ApiError / ApiResult — the public error envelope
  auth.rs                 ScopeSet, AuthCtx, AuthCache, require_auth middleware
  extract.rs              Json/Query/Path wrappers that keep the envelope
  pagination.rs           ListParams → PageRequest → Page → json
  validate.rs             edge-only validation (cvc, webhook_url, enabled_events, idempotency_key)
  middleware/
    mod.rs
    request_id.rs         x-request-id adoption/generation + task-local
    timeout.rs            wall-clock budget → 504 in the envelope
    idempotency.rs        the Idempotency-Key protocol
  routes/
    mod.rs                router() assembly, CORS, fallbacks, panic handler, rfc3339()
    health.rs             /health, /health/live, /health/ready
    orders.rs             /v1/orders
    payments.rs           /v1/payments  ← the only module touching cardholder data
    webhook_endpoints.rs  /v1/webhook_endpoints
    events.rs             /v1/events
  tests/
    http_api.rs           36 black-box integration tests over the real router
```

`lib.rs` re-exports: `ApiConfig`, `ApiError`, `ApiResult`, `router`, `AppState`.

`lib.rs` carries `#![allow(clippy::result_large_err)]` with a written rationale:
`ApiError` holds three owned strings; boxing it would cost an allocation on every
failure, which is a bad trade for a type that only exists on the cold path.

### 4.3 Adding a new resource — the pattern

1. Create `crates/api/routes/<resource>.rs`.
2. Write a module-level doc comment saying *why* this module exists and what is
   special about it.
3. Export `pub fn routes() -> Router<Arc<AppState>>` returning only the routes,
   **no layers** — layers are composed once in `routes/mod.rs`.
4. Handlers take `State(state): State<Arc<AppState>>`,
   `Extension(auth): Extension<AuthCtx>`, and `crate::extract::{Json, Query, Path}`
   (never `axum::extract::Json`).
5. First line of every handler: `auth.require(Scope::X)?;`
6. Request DTOs derive `Deserialize` with `#[serde(deny_unknown_fields)]`.
7. Parse ids with `.parse().map_err(|_| ApiError::not_found("<resource>"))?` —
   an unparseable id is a 404, not a 400.
8. Serialise with a private `fn render(&T) -> Value` (or `engine::event::*_json`),
   so there is one projection per resource.
9. Add `pub mod <resource>;` and `.merge(<resource>::routes())` in `routes/mod.rs`.
10. Add unit tests in the same file (rendering, mapping, redaction) and black-box
    tests in `tests/http_api.rs` (status codes, envelope, isolation).

---

## 5. Router assembly and middleware order

`crates/api/routes/mod.rs`:

```rust
pub fn router(state: Arc<AppState>) -> Router {
    let cors = cors_layer();

    let authed = Router::new()
        .merge(orders::routes())
        .merge(payments::routes())
        .merge(webhook_endpoints::routes())
        .merge(events::routes())
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::middleware::from_fn_with_state(state.clone(), idempotency::enforce))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::require_auth));

    Router::new()
        .merge(health::routes())
        .nest("/v1", authed)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(state.config.max_body_bytes))
        .layer(axum::middleware::from_fn_with_state(state.clone(), timeout::enforce))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(axum::middleware::from_fn(request_id::propagate))
        .layer(SetSensitiveRequestHeadersLayer::new([AUTHORIZATION, COOKIE]))
        .layer(cors)
        .with_state(state)
}
```

**In tower, the LAST `.layer()` is the OUTERMOST.** Effective order:

```
CORS
 └─ sensitive-header redaction (Authorization, Cookie never printed)
     └─ request_id            (adopt or generate; sets task-local; echoes header)
         └─ CatchPanic        (a panic becomes a 500 in the envelope)
             └─ Trace         (spans/logs carry the request id)
                 └─ timeout   (wall-clock budget → 504)
                     └─ body limit (413)
                         └─ [/v1 only] auth        (401/403)
                             └─ [/v1 only] idempotency (needs AuthCtx)
                                 └─ handler
```

Why this order:

- **request_id outside CatchPanic and Trace** — a panic's 500 and every trace line
  must carry the id.
- **CatchPanic outside Trace** — a panic still produces a trace record.
- **timeout inside CatchPanic** — a timeout is a normal response, not a panic.
- **auth outside idempotency** — idempotency keys are scoped per merchant, so
  `idempotency::enforce` requires an `AuthCtx` in extensions. If it is missing it
  logs "check the layer order" and fails closed with a 500.
- **`/health*` sits outside `/v1`** so probes need no key and no idempotency.

CORS is permissive-origin (`Any`) **without credentials** — an API-key API has no
cookies to protect. Methods GET/POST/DELETE/OPTIONS; request headers
`authorization`, `content-type`, `idempotency-key`, `x-request-id`; exposed
headers `x-request-id`, `idempotent-replayed`, `retry-after`; `max_age` 600s.

Fallbacks: `not_found` → `ApiError::unknown_endpoint(method, path)`;
`method_not_allowed` → `ApiError::method_not_allowed(method, path)`. Both are
registered on the inner and outer routers so a bad `/v1` path is still an
envelope (and, being inside auth, a bad `/v1` path without a key is a **401**, not
a 404 — that is intentional).

`handle_panic` logs the payload detail and returns a generic 500 that leaks
nothing.

`pub(crate) fn rfc3339(t: OffsetDateTime) -> String` lives here; every route
module uses it so timestamps are formatted identically.

---

## 6. The error envelope (public contract)

Exactly one error shape, for every failure, everywhere:

```json
{
  "error": {
    "type": "invalid_request_error",
    "code": "currency_invalid",
    "message": "'XYZ' is not a supported currency.",
    "param": "currency",
    "request_id": "req_0192f3..."
  }
}
```

Optional extras: `payment_id` on card errors; a `Retry-After` header on
503/504/429-shaped errors.

`ApiError` fields: `status`, `err_type`, `code`, `message`, `param`,
`payment_id`, `retry_after_secs`.

### Constructors (use these, not `ApiError` literals)

| Constructor | Status | Use for |
|---|---|---|
| `invalid_request(code, msg, param)` | 400 | malformed or unacceptable input |
| `not_found(resource)` | 404 | missing **or** not owned by this merchant |
| `unauthorized(msg)` | 401 | missing/bad key |
| `forbidden(msg)` | 403 | authenticated but lacking scope |
| `unknown_endpoint(method, path)` | 404 | router fallback |
| `method_not_allowed(method, path)` | 405 | method fallback |
| `conflict(code, msg)` | 409 | idempotency mismatch/in-flight, unique violations |
| `payload_too_large(limit)` | 413 | body limit |
| `unsupported_media_type()` | 415 | missing/wrong `Content-Type` |
| `card(code, msg, payment_id)` | 402 | declines, unknown test card, bad CVC |
| `unavailable()` | 503 | dependency down; sets `Retry-After` |
| `timeout()` | 504 | request budget exceeded |
| `internal()` / `internal_from(err, ctx)` | 500 | anything unexpected; `internal_from` logs the detail |

Also: `.with_param(..)`, `.retry_after(secs)`, `.is_server_fault()`.

### Never do this

```rust
// WRONG — throws away a mapping that already exists and turns a fixable
// 409 into an unfixable 500.
something().await.map_err(|_| ApiError::internal())?;
```

Use the `From` impls. `From<EngineError>` currently maps:

```rust
EngineError::NotFound { resource }        => ApiError::not_found(resource)
EngineError::Store(s)                     => ApiError::from(s)   // preserves 409 etc.
EngineError::Validation { field, reason } => 400 parameter_invalid, param = field
EngineError::Card(c)                      => 402 card_invalid
EngineError::OrderTransition(t)           => 400 invalid_state
EngineError::PaymentTransition(t)         => 400 invalid_state
EngineError::Money(m)                     => 400 amount_invalid, param = "amount"
other                                     => internal_from(other, "engine")
```

The `Store(s) => ApiError::from(s)` arm exists because of a real bug: a duplicate
`receipt` (unique index `orders_merchant_receipt_idx`) was being swallowed by the
catch-all and returned as a 500 instead of a 409. Regression test:
`a_duplicate_receipt_is_a_409_not_a_500`.

### Extractors

`crate::extract::{Json, Query, Path}` wrap the axum originals and convert every
rejection into the envelope — malformed JSON → `json_invalid` (400), missing
`Content-Type` → 415, unknown field / wrong type → 400 with `param` naming the
**nested** path (e.g. `card.exp_month`), oversized body → 413. Handlers must
never use `axum::extract::Json` directly for request bodies. (`axum::Json` for
*responses* is fine — `health.rs` uses it.)

---

## 7. Auth

- Header: `Authorization: Bearer sk_test_...`. Only secret test keys are valid.
  A publishable key (`pk_test_`) gets its own 401 message explaining that it
  belongs in browser code, not here; a live key (`sk_live_`/`pk_live_`) is
  rejected outright. 401s carry a `WWW-Authenticate: Bearer realm="payment-sandbox"`
  challenge, with `error="invalid_token"` / `error="insufficient_scope"` where
  those apply.
- `ScopeSet` is a `u16` bitset over `domain::api_key::Scope`:
  `orders:read`, `orders:write`, `payments:read`, `payments:write`,
  `refunds:write`, `webhooks:manage`.
- `AuthCtx { merchant_id, key_id, scopes }` is `Copy`, inserted into request
  extensions by `require_auth`, read by handlers via `Extension(auth)`.
- `auth.require(scope)?` → 403; also `require_all`, `require_any`.
- `AuthCache` memoises key lookups (default TTL 5s, capacity 4096, `last_used_at`
  touch interval 60s) using a **monotonic** `Instant`, so a clock change cannot
  extend a cache entry's life. **Any handler that revokes a key must call
  `AuthCache::invalidate_key`.** `AuthCache::disabled()` exists for tests.

Scope per route: orders read/write, payments read/write, `/events` requires
`payments:read` (diagnostic data should not require a write-capable key),
webhook endpoints require `webhooks:manage`.

---

## 8. Idempotency protocol

Header: `Idempotency-Key: <8–255 printable ASCII>` on mutating requests
(POST/PUT/PATCH/DELETE). Reads pass straight through. **Requests without the
header also pass through** — a deliberate availability choice for a sandbox that
must stay usable from `curl`; a production deployment would make it required on
`/v1/payments`.

Decided by one atomic `INSERT ... ON CONFLICT DO NOTHING` in
`store::idempotency`:

| Outcome | Meaning | Response |
|---|---|---|
| `Acquired` | first time | execute, then record the response |
| `Replay` | completed, same request fingerprint | the stored response verbatim + `idempotent-replayed: true` |
| `Mismatch` | completed, **different** fingerprint | 409 — a client bug, never be lenient here |
| `InFlight` | still executing | 409 `idempotency_key_in_flight` |

- The fingerprint covers **method + path + body**, deliberately **not headers**
  (a proxy adding a trace header must not turn a retry into a 409).
- Keys are scoped per merchant.
- Store unreachable → 503, never proceed unprotected.
- Handler returns 5xx → the key is **released** so a retry re-executes.
- Response bodies larger than `max_recorded_response_bytes` are returned but not
  recorded (logged as such).

---

## 9. Pagination

Cursor-based, uniform across all collections.

Query: `?limit=<1..=max_page_size>&before=<RFC3339>`.
`ListParams` uses `deny_unknown_fields`, so `?limitt=5` is a 400 rather than a
silently-default page. An out-of-range `limit` is **rejected** with a message
naming the maximum, not clamped.

Flow inside a handler:

```rust
let page = params.resolve(&state.config)?;          // → PageRequest
let rows = store::x::list_for_merchant(&pool, mid, page.before, page.fetch_limit()).await?;
Ok(Json(page.finish(rows, render, |r| r.created_at).into_json()))
```

`fetch_limit()` = `limit + 1`; the extra row's existence answers `has_more`.
Response shape:

```json
{ "object": "list", "data": [ ... ], "has_more": true, "next_before": "2026-01-01T00:00:00Z" }
```

Defaults: `default_page_size` 10, `max_page_size` 100.
`engine::order::MAX_LIST_LIMIT = 200` clamps at the engine boundary as a second
line of defence.

---

## 10. Configuration (`ApiConfig`)

Every operational limit lives in `crates/api/config.rs`. These are **defence in
depth, not business rules** — business rules belong in `engine`.

| Field | Default |
|---|---|
| `max_body_bytes` | 256 KiB |
| `max_recorded_response_bytes` | 256 KiB |
| `request_timeout` | 30s |
| `idempotency_ttl` | 24h |
| `default_page_size` | 10 |
| `max_page_size` | 100 |
| `simulate_latency` | true (server), false in tests |
| `max_simulated_latency` | 5s |

`ApiConfig::for_tests()` = defaults with `simulate_latency: false` — every limit
stays enforced. A unit test asserts the defaults are internally consistent
(`default_page_size <= max_page_size`, `max_simulated_latency < request_timeout`).

Server env vars (`sandbox-server`, via envconfig):
`DATABASE_URL`, `SERVER_HOST` (default `0.0.0.0`), `SERVER_PORT` (default `8080`),
`CRYPTO_CARD_FINGERPRINT_PEPPER`, `CRYPTO_MASTER_KEY`, `SIMULATE_LATENCY`
(default `true`).

---

## 11. Endpoints (current surface)

Unauthenticated:

```
GET  /health         deep  (SELECT 1) → 200 {"status":"ok"} | 503 {"status":"database_unreachable"}
GET  /health/live    trivial, never touches a dependency
GET  /health/ready   deep
```
All three send `Cache-Control: no-store`. Liveness is trivial on purpose — a
liveness probe that fails on a database blip gets the container killed, which
helps nothing and drops in-flight payments. The 503 body names no internals.

Authenticated, under `/v1`:

```
POST   /v1/orders                     orders:write
GET    /v1/orders                     orders:read   (paginated)
GET    /v1/orders/{id}                orders:read
POST   /v1/payments                   payments:write
GET    /v1/payments/{id}              payments:read
POST   /v1/payments/{id}/capture      payments:write
POST   /v1/webhook_endpoints          webhooks:manage  (returns signing_secret ONCE)
GET    /v1/webhook_endpoints          webhooks:manage  (unpaginated, capped at 16)
DELETE /v1/webhook_endpoints/{id}     webhooks:manage  (disable; idempotent)
GET    /v1/events                     payments:read    (paginated)
```

### Payment creation flow (the important one)

```
auth.require(PaymentsWrite)
  → validate::cvc (shape only, then dropped)
  → parse OrderId          (bad id ⇒ 404 "order", never 400)
  → state.orders.get(merchant, order_id)   (ownership check)
  → simulator::decide(&pan, order.amount)  ← GATE: unknown card ⇒ 402
                                             card_not_in_test_set, NOTHING written
  → optional sleep: min(decision.latency_ms, config.max_simulated_latency)
  → state.payments.create(..., map_decision(decision))   [engine transaction]
  → if status == Failed ⇒ 402 card error WITH payment_id attached
  → else 201 engine::event::payment_json(&payment)
```

A decline **commits** the failed payment row first, *then* reports the error with
`payment_id` — the merchant can always fetch what happened.

`CardRequest` has a **hand-written `Debug`** that renders `number` and `cvc` as
`[redacted]` (not even last-four: the type exists before validation, so the field
may hold anything). Never derive `Debug` on a type holding a PAN. Tests assert
the redaction and that every `simulator::Outcome` maps to an `AttemptOutcome`.

### Webhook endpoints

`MAX_ACTIVE_ENDPOINTS = 16` per merchant — a blast-radius control, since every
event fans out to every active endpoint. The cap is checked *before* minting a
secret so a rejected request leaves no trace. `render()` never emits
`signing_secret`; creation sets `body["signing_secret"]` by hand at the one call
site. DELETE disables (`disabled_at`) rather than deleting, because undelivered
outbox jobs reference the row; disabling an already-disabled endpoint is a no-op
success.

---

## 12. Validation — where each check lives

- **`domain`** — what is true of a value forever (Luhn-valid PAN, non-negative
  amount, currency).
- **`engine`** — business rules (minimum order amount, note limits), so they hold
  no matter which transport called in.
- **`crates/api/validate.rs`** — only what is meaningful *because it arrived over
  HTTP*: `cvc` (shape; 402 not 400, because the card data is what is wrong),
  `webhook_url` (parsed with `url::Url`, max 2048), `enabled_events` (max 32,
  each ≤ 64 chars, deduped), `idempotency_key` (8–255 printable ASCII).

**Never re-check in `api` something `engine` already guarantees.** If the error
message is bad, improve the engine's message — do not add a second copy of the
rule that will drift.

---

## 13. Database

Postgres on **54432**. `compose.yaml` at the repo root brings it up.

21 migration up/down pairs in `migrations/`: extension, merchants, api_keys,
orders, payment_methods, payments, payment_attempts, refunds, disputes,
ledger_accounts, ledger_transactions, ledger_entries, ledger_constraints, events,
webhook_endpoints, webhook_deliveries, jobs, settlements, audit_logs,
idempotency_keys, seed accounts.

sqlx runs in **offline mode** against the checked-in `.sqlx/` directory. If you
change any `sqlx::query!`/`query_scalar!` **string**, you must regenerate:

```bash
DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev cargo sqlx prepare --workspace
```

(This is why `health.rs` keeps the exact literal `"SELECT 1 AS one"` — changing
the text invalidates the cached entry.)

---

## 14. Build, test, lint

```bash
export DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev

cargo build --workspace
cargo test  --workspace          # integration tests need the DB up
cargo fmt --all
cargo clippy --all-targets --all-features -- \
  -D warnings -D clippy::unwrap_used -D clippy::expect_used -D clippy::todo \
  -D clippy::float_arithmetic -D clippy::cast_possible_truncation -D clippy::cast_sign_loss
```

That clippy line is exactly what `.github/workflows/ci.yaml` runs — match it
locally before claiming a change is clean.

`clippy.toml` sets `allow-unwrap-in-tests = true` and `allow-expect-in-tests = true`,
but **those only apply inside `#[test]` functions and `#[cfg(test)]` modules.**
Module-level helpers in an integration-test binary (`tests/*.rs`) are not covered,
which is why `crates/api/tests/http_api.rs` carries a file-scoped
`#![allow(clippy::unwrap_used, clippy::expect_used)]` with a comment explaining
why. Do **not** loosen the workspace lints to fix this.

`float_arithmetic` is denied because money is integer minor units. There is no
floating-point arithmetic anywhere in this codebase, and there must not be.

Current test counts (all green): api 111 unit + 36 http · crypto 40 · domain 87 ·
engine 18 · queue 9 · simulator 18 · store 66 · webhooks 3.

---

## 15. House style — match this or the code will look foreign

1. **Module-level doc comments explain *why*, not *what*.** Every non-trivial
   module opens with several paragraphs on what problem it solves, what the
   alternative was, and what breaks if you do it the other way. Read
   `middleware/idempotency.rs` or `middleware/timeout.rs` for the register.
2. **Inline comments record decisions and tradeoffs**, especially at the point
   where a reader would reasonably ask "why not the obvious thing?"
3. **Tests are named as sentences**, e.g.
   `a_duplicate_receipt_is_a_409_not_a_500`,
   `debug_output_never_contains_the_pan_or_the_cvc`,
   `liveness_answers_without_a_database_and_forbids_caching`.
   Each test proves one behaviour and its name states that behaviour.
4. **Unit tests live in `#[cfg(test)] mod tests` at the bottom of the file they
   test.** Black-box HTTP tests go in `crates/api/tests/http_api.rs`.
5. **Constants are named and documented**, never spelled inline at a call site.
6. `#[must_use]` on pure constructors/accessors.
7. Errors are `thiserror` enums per crate; `anyhow` only in `sandbox-server`.
8. Prefer `Duration`/`OffsetDateTime` over integer seconds in signatures.
9. When something is deliberately not done, say so in a comment with the reason
   (see the `# Known limitation` block in `pagination.rs`).

---

## 16. Known open items (do not "discover" these; they are already tracked)

### 16.1 A timed-out request can wedge its idempotency key

`timeout::enforce` sits **outside** `idempotency::enforce`. On timeout,
`tokio::time::timeout` **drops** the inner future, so idempotency's
`record()`/`release()` never run after `acquire()` already wrote the row as
`in_progress`. `store::idempotency` only clears `in_progress` rows in
`purge_expired` at `expires_at` (24h), so retries get
`409 idempotency_key_in_flight` for a day — which contradicts the 504 message
telling the caller to *"Retry with the same Idempotency-Key to find out whether it
completed."*

**Do not fix this by moving the timeout inside idempotency** — that would release
the key on 504 and reopen the double-charge window. The intended fix is to bound
the stale window: let `store::idempotency::acquire` take over an `in_progress`
row older than the request budget plus a margin, driven by a new
`stale_in_flight_after` field on `ApiConfig`. Not yet implemented (it touches the
`store` crate).

### 16.2 Pagination cursor is `created_at` only

Rows sharing a `created_at` to the microsecond can be skipped at a page boundary.
The fix is a composite `(created_at, id)` cursor in the store's `WHERE` clause
plus a matching index. Deliberately deferred; documented in `pagination.rs`.

### 16.3 Housekeeping

`crates/store/src/Untitled-1.rs` is a stray file that should be removed.

---

## 17. Quick smoke test

```bash
# 1. DB up on 54432
docker compose up -d
export DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev

# 2. run
SERVER_PORT=8099 cargo run -p sandbox-server

# 3. probe
curl -i localhost:8099/health                       # 200 + Cache-Control: no-store
curl -i localhost:8099/v1/orders                    # 401 in the envelope
curl -i -X PUT localhost:8099/health                # 405 in the envelope
curl -i localhost:8099/v1/nope -H 'Authorization: Bearer sk_...'   # 404 unknown_endpoint

curl -s localhost:8099/v1/orders \
  -H 'Authorization: Bearer sk_...' \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: order-demo-0001' \
  -d '{"amount":50000,"currency":"INR","receipt":"rcpt_1"}'
# repeat the exact same call → same body, plus `idempotent-replayed: true`
```

---

## 18. Checklist before you call a change done

- [ ] `cargo fmt --all` clean
- [ ] The full CI clippy line (§14) passes with zero warnings
- [ ] `cargo test --workspace` green
- [ ] `.sqlx/` regenerated if any query string changed
- [ ] New route registered in `routes/mod.rs` and scope-gated
- [ ] Every new error path returns the envelope (§6) with a stable `code`
- [ ] No PAN/CVC/secret can reach a log, a `Debug`, or a response
- [ ] Ownership failures return 404, not 403
- [ ] New module has a "why" doc comment; new tests are named as sentences
- [ ] Anything deliberately not done is written down as a comment
