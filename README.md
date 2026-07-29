<div align="center">

# payment-sandbox

**A production-shaped payment gateway, written in Rust, that moves no money.**

Test cards in, realistic gateway behaviour out — authorizations, captures, declines, partial
refunds, a double-entry ledger that provably balances, a transactional outbox, and signed
webhooks with exponential-backoff retries.

[![CI](https://img.shields.io/badge/CI-fmt%20%C2%B7%20clippy%20%C2%B7%20test-brightgreen)](.github/workflows/ci.yaml)
[![Rust](https://img.shields.io/badge/rust-2021-orange)](https://www.rust-lang.org)
[![axum](https://img.shields.io/badge/axum-0.8.9-blue)](https://github.com/tokio-rs/axum)
[![sqlx](https://img.shields.io/badge/sqlx-0.9%20offline-blue)](https://github.com/launchbadge/sqlx)
[![unsafe](https://img.shields.io/badge/unsafe-forbidden-success)](#security-model)
[![tests](https://img.shields.io/badge/tests-398%20passing-brightgreen)](#testing)

</div>

---

## The one-sentence version

A merchant should be able to integrate against this sandbox, then swap the base URL for a real
gateway and change nothing else.

That north star drives every decision in this repository. It is why the error envelope is a
public contract rather than whatever `axum` happened to return, why cross-merchant access is a
`404` and not a `403`, why idempotency keys are enforced with a single atomic
`INSERT ... ON CONFLICT`, and why the ledger has a deferred database trigger that refuses to
commit an unbalanced transaction.

It is a sandbox, so no money moves. It is **not a toy**.

---

## Table of contents

| | |
|---|---|
| [Quickstart](#quickstart) · [Architecture](#architecture) · [Request lifecycle](#request-lifecycle) | Get it running, then understand it |
| [API reference](#api-reference) · [Test cards](#test-cards) · [Error envelope](#the-error-envelope) | The wire contract |
| [Authentication](#authentication) · [Idempotency](#idempotency) · [Pagination](#pagination) | Cross-cutting protocols |
| [Webhooks](#webhooks) · [The ledger](#the-double-entry-ledger) · [Fees](#fees) | The interesting subsystems |
| [Data model](#data-model) · [Security model](#security-model) · [Configuration](#configuration) | Operations |
| [Development](#development) · [Testing](#testing) · [Dashboard](#dashboard-in-progress) · [Roadmap](#roadmap) | Contributing and what's next |

---

## Quickstart

Three commands and you have a working gateway. **Postgres runs on port `54432`, never 5432** —
that is deliberate, so this never collides with a Postgres you already have running.

### 1. Bring up the database

```bash
docker compose up -d
export DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev
```

### 2. Mint a merchant and an API key

```bash
cargo run -p sandbox-server -- seed
```

```text
Using existing demo merchant.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Merchant   mer_019fa92b-9aa4-7303-b053-eb1954ee8f8b

  Secret key      sk_test_ae304eb4...REDACTED...03621c18
  Publishable key pk_test_ee4fd3f5...REDACTED...a06323b5

  These are shown ONCE. Only their hashes are stored.
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Try it (server must be running):

  curl -s -X POST localhost:8080/v1/orders \
    -H "Authorization: Bearer sk_test_ae304eb4...REDACTED...03621c18" \
    -H "Content-Type: application/json" \
    -d '{"amount": 150000, "currency": "INR", "receipt": "demo_1"}'

  # then pay it (use the order id from the response):

  curl -s -X POST localhost:8080/v1/payments \
    -H "Authorization: Bearer sk_test_ae304eb4...REDACTED...03621c18" \
    -H "Content-Type: application/json" \
    -d '{"order_id": "<ORDER_ID>", "card": {"number": "4242424242424242", "exp_month": 12, "exp_year": 2030, "cvc": "123"}}'

Test cards: 4242424242424242 succeeds · 4000000000009995 insufficient funds
            4000000000000077 authorize-only · full table in the docs
```

> **The plaintext key is printed once and never again.** Only an Argon2id hash reaches the
> database, so there is no query that can recover it. Lost it? Run `seed` again — it reuses the
> demo merchant and mints an additional key. Old keys keep working until revoked.
>
> Every transcript in this README is real captured output from a local run against a local
> Postgres. **Key material is the one thing shortened** — secret keys and webhook signing secrets
> appear as `sk_test_ae304eb4...REDACTED...03621c18` rather than in full, so that copying this file
> around never circulates a working credential and secret scanners do not flag the repository. Ids,
> amounts, timestamps, signatures and ledger figures are verbatim.
>
> Export your own key once and the rest of the examples work as written:
>
> ```bash
> export SK=sk_test_...   # the secret key seed just printed
> ```

### 3. Run the server

```bash
SERVER_PORT=8099 cargo run -p sandbox-server
```

```text
INFO migrations applied
INFO payment sandbox listening — TEST MODE, no real money addr=0.0.0.0:8099
```

### 4. Take a payment, end to end

<details open>
<summary><b>Create an order</b></summary>

```bash
curl -i -X POST localhost:8099/v1/orders \
  -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: readme-order-0001' \
  -d '{"amount":150000,"currency":"INR","receipt":"rcpt_readme_1","notes":{"customer":"Ada Lovelace"}}'
```

```http
HTTP/1.1 201 Created
content-type: application/json
x-request-id: req_019fac72048d7100b9b3ed167ac8beb2
access-control-expose-headers: x-request-id,idempotent-replayed,retry-after
access-control-allow-origin: *

{
  "id": "order_019fac72-0499-7602-a486-a4d7e88780a2",
  "amount": 150000,
  "amount_paid": 0,
  "currency": "INR",
  "receipt": "rcpt_readme_1",
  "status": "created",
  "notes": { "customer": "Ada Lovelace" },
  "created_at": "2026-07-29T05:56:21.273122Z"
}
```

`150000` is **paise** — ₹1,500.00. Every amount on this API is an integer in the currency's
minor unit. There is no floating-point arithmetic anywhere in this codebase, and CI denies
`clippy::float_arithmetic` to keep it that way.

</details>

<details open>
<summary><b>Pay it with the success card</b></summary>

```bash
curl -i -X POST localhost:8099/v1/payments \
  -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: readme-pay-0002' \
  -d '{"order_id":"order_019fac72-0499-7602-a486-a4d7e88780a2",
       "card":{"number":"4242424242424242","exp_month":12,"exp_year":2030,"cvc":"123"}}'
```

```http
HTTP/1.1 201 Created
content-type: application/json
x-request-id: req_019fac72dc507c31bd2597adcfcfd12f

{
  "id": "pay_019fac72-dc57-76e1-985d-fed01664fc9d",
  "order_id": "order_019fac72-0499-7602-a486-a4d7e88780a2",
  "amount": 150000,
  "currency": "INR",
  "amount_refunded": 0,
  "status": "captured",
  "method": "card",
  "card": { "last4": "4242", "brand": "visa", "exp_month": 12, "exp_year": 2030 },
  "error_code": null,
  "error_description": null,
  "notes": {},
  "created_at": "2026-07-29T05:57:16.500121Z",
  "captured_at": "2026-07-29T05:57:16.500121Z"
}
```

The card number is gone. It was Luhn-checked, matched against the published test set,
fingerprinted with a peppered hash, reduced to `last4` + `brand`, and dropped. The CVC was
shape-checked and dropped — no struct in `engine` or `store` even has a field for it.

</details>

<details open>
<summary><b>The order is now paid</b></summary>

```bash
curl -s localhost:8099/v1/orders/order_019fac72-0499-7602-a486-a4d7e88780a2 \
  -H "Authorization: Bearer $SK"
```

```json
{
  "id": "order_019fac72-0499-7602-a486-a4d7e88780a2",
  "amount": 150000,
  "amount_paid": 150000,
  "currency": "INR",
  "receipt": "rcpt_readme_1",
  "status": "paid",
  "notes": { "customer": "Ada Lovelace" },
  "created_at": "2026-07-29T05:56:21.273122Z"
}
```

</details>

<details open>
<summary><b>Refund it</b></summary>

Omit `amount` and you refund the entire remaining balance — resolved under the payment's row
lock, so it cannot race another refund landing between your read and your write.

```bash
curl -i -X POST localhost:8099/v1/refunds \
  -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: readme-refund-0001' \
  -d '{"payment_id":"pay_019fac72-dc57-76e1-985d-fed01664fc9d","reason":"customer changed their mind"}'
```

```http
HTTP/1.1 201 Created
x-request-id: req_019fac72dd0073e282eeb68ec263b3c2

{
  "id": "re_019fac72-dd0c-7e83-bca7-702ddbca890c",
  "object": "refund",
  "payment_id": "pay_019fac72-dc57-76e1-985d-fed01664fc9d",
  "amount": 150000,
  "currency": "INR",
  "reason": "customer changed their mind",
  "status": "processed",
  "created_at": "2026-07-29T05:57:16.675568Z",
  "processed_at": "2026-07-29T05:57:16.675568Z"
}
```

</details>

That is the whole loop: **order → payment → refund**, with a balanced ledger, four events, and a
signed webhook fired for each one, all in about ten seconds of work.

---

## Architecture

Nine crates, Rust 2021, resolver 2. The dependency graph is **strictly one-way** and enforced by
`Cargo.toml` — you cannot introduce a cycle without a compile error.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                            sandbox-server                                │
│         the binary · env config · wiring · graceful shutdown             │
│                 the ONLY crate allowed to use anyhow                     │
└───────────────────────────────┬──────────────────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────────────────┐
│                                api                                       │
│  axum router · middleware stack · extractors · error envelope · auth     │
│      the ONLY place simulator and engine vocabularies are translated     │
└──────────┬──────────────────────────────────────────┬────────────────────┘
           │                                          │
┌──────────▼─────────────┐                 ┌──────────▼───────────────────┐
│        engine          │                 │         simulator            │
│  orchestration · owns  │                 │  test-card → outcome table   │
│  transactions · outbox │   (no edge —    │  pure, deterministic, no I/O │
│  ledger · business     │    engine does  │                              │
│  rules                 │    NOT depend   └──────────────────────────────┘
└──────────┬─────────────┘    on simulator)
           │
┌──────────▼─────────────┐   ┌──────────────────┐   ┌──────────────────────┐
│         store          │   │      queue       │──▶│      webhooks        │
│  sqlx · row structs    │   │  jobs table      │   │  sign · POST · retry │
│  StoreError · tx locks │   │  worker loop     │   │                      │
└──────────┬─────────────┘   └──────────────────┘   └──────────────────────┘
           │
┌──────────▼─────────────┐   ┌──────────────────────────────────────────────┐
│        domain          │◀──│                   crypto                     │
│  pure types + invari-  │   │  Argon2id · peppered fingerprints · HMAC     │
│  ants · no I/O, no     │   │  AES-GCM · key generation                    │
│  sqlx, no axum         │   └──────────────────────────────────────────────┘
└────────────────────────┘
```

### Why the graph looks like this

**`domain` depends on nothing in the workspace.** Money, currencies, card validation, state
machines and the chart of accounts are all pure. They can be unit-tested at millions of cases per
second with `proptest`, and no test needs a database.

**`engine` does not depend on `simulator`.** This is the subtle one. The simulator's vocabulary
(`Outcome::Success`, `Outcome::RiskHold`) is translated into the engine's vocabulary
(`AttemptOutcome::Capture`, `AttemptOutcome::RiskHold`) by `map_decision` in
[crates/api/routes/payments.rs](crates/api/routes/payments.rs). Keeping that edge out of the
graph means every engine outcome — including the ones no card produces — can be tested without a
simulator in sight, and it keeps the graph acyclic.

**`anyhow` is confined to `sandbox-server`.** Library crates use `thiserror` and typed errors, so
a caller can always match on what went wrong. Errors become opaque exactly once, at the point
they become an exit code.

### Crate responsibilities

| Crate | Responsibility | Tests |
|---|---|---|
| [`domain`](crates/domain/) | Types and invariants that are true forever: `Money`, `Currency`, Luhn, `PaymentStatus`, `OrderStatus`, the chart of accounts | 88 |
| [`store`](crates/store/) | Every `sqlx` query, row structs, `StoreError`, transaction helpers, documented lock ordering | 66 |
| [`crypto`](crates/crypto/) | Argon2id key hashing, peppered card fingerprints, HMAC-SHA256 signing, AES-GCM, key generation | 40 |
| [`queue`](crates/queue/) | Job primitives over the `jobs` table, the worker loop, the retry ladder | 9 |
| [`simulator`](crates/simulator/) | The card table and the decision function. Pure, deterministic, zero I/O | 18 |
| [`engine`](crates/engine/) | Orchestration: owns transactions, business rules, the outbox, ledger postings, fee schedules | 24 |
| [`webhooks`](crates/webhooks/) | Delivery worker: signs, POSTs, records the attempt, schedules the retry | 3 |
| [`api`](crates/api/) | The HTTP surface: router, middleware, extractors, error envelope, auth cache | 114 + 36 |
| [`sandbox-server`](crates/sandbox-server/) | The binary: env config, wiring, the `seed` command, graceful shutdown | — |

> **Note:** `crates/api/Cargo.toml` sets `[lib] path = "lib.rs"`, so the `api` crate's sources
> live at the crate root rather than under `src/`. Every other crate uses `src/`.

---

## Request lifecycle

Every request passes through the same stack. In `tower` the **last** `.layer()` is the
**outermost**, which is famously easy to get backwards, so the effective order is written down
here and in [crates/api/routes/mod.rs](crates/api/routes/mod.rs).

```
   ┌─ CORS ──────────────────────────────────────────────────────────────┐
   │  permissive origin, no credentials (an API-key API has no cookies)  │
   │ ┌─ sensitive headers ─────────────────────────────────────────────┐ │
   │ │  Authorization and Cookie marked before tracing ever reads them │ │
   │ │ ┌─ request_id ────────────────────────────────────────────────┐ │ │
   │ │ │  adopt x-request-id or generate one; set task-local; echo   │ │ │
   │ │ │ ┌─ CatchPanic ────────────────────────────────────────────┐ │ │ │
   │ │ │ │  a panic becomes a 500 in the envelope, leaking nothing │ │ │ │
   │ │ │ │ ┌─ Trace ─────────────────────────────────────────────┐ │ │ │ │
   │ │ │ │ │  spans and logs carry the request id                │ │ │ │ │
   │ │ │ │ │ ┌─ timeout ───────────────────────────────────────┐ │ │ │ │ │
   │ │ │ │ │ │  30s wall-clock budget → 504 in the envelope    │ │ │ │ │ │
   │ │ │ │ │ │ ┌─ body limit ────────────────────────────────┐ │ │ │ │ │ │
   │ │ │ │ │ │ │  256 KiB → 413, refused before buffering   │ │ │ │ │ │ │
   │ │ │ │ │ │ │ ┌───────── /v1 only ──────────────────────┐ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ ┌─ auth ─────────────────────────────┐  │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │  401 / 403, WWW-Authenticate       │  │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ ┌─ idempotency ──────────────────┐ │  │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ │  needs AuthCtx → must be inside│ │  │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ │ ┌─ handler ──────────────────┐ │ │  │ │ │ │ │ │ │ │
   │ │ │ │ │ │ │ │ │ │ │  auth.require(Scope::X)?;  │ │ │  │ │ │ │ │ │ │ │
   └─┴─┴─┴─┴─┴─┴─┴─┴─┴─┴────────────────────────────┴─┴─┴──┴─┴─┴─┴─┴─┴─┴─┘
```

Each placement is load-bearing:

- **`request_id` outside `CatchPanic` and `Trace`** — a panic's 500 and every trace line must
  carry the correlation id, so it has to be established before either can fire.
- **`CatchPanic` outside `Trace`** — a panic still produces a trace record rather than vanishing.
- **`timeout` inside `CatchPanic`** — a timeout is a normal response, not a panic.
- **`auth` outside `idempotency`** — idempotency keys are scoped per merchant, so
  `idempotency::enforce` needs an `AuthCtx` in request extensions. If it is missing, it logs
  "check the layer order" and fails closed with a 500 rather than proceeding unprotected.
- **`/health*` sits outside `/v1`** — an orchestrator probing readiness has no API key, and a
  version prefix on a probe URL is a migration hazard.

### Nothing escapes the envelope

Unmatched paths, unmatched methods, rejected extractors, oversized bodies, timeouts and panics
all funnel into `ApiError`. A `404` under `/v1` sent **without** a key returns `401`, not `404` —
that is intentional, and it means an unauthenticated caller cannot map which paths exist.

```bash
curl -s localhost:8099/v1/nope -H "Authorization: Bearer $SK"
```
```json
{"error":{"type":"invalid_request_error","code":"unknown_endpoint",
  "message":"GET /nope is not an endpoint on this API.","param":null,
  "request_id":"req_019fac747c2e73b1b2d66da01266bc88"}}
```

```bash
curl -s -X PUT localhost:8099/health
```
```json
{"error":{"type":"invalid_request_error","code":"method_not_allowed",
  "message":"PUT is not supported on /health.","param":null,
  "request_id":"req_019fac747c5c7bf1b055b96b0d7dff71"}}
```

```bash
curl -s -o /dev/null -w 'HTTP %{http_code}\n' localhost:8099/v1/nope   # no key
# HTTP 401
```

### The correlation id

Every response carries `x-request-id`. Send your own and it is adopted, so a trace id from your
own system flows straight through:

```bash
curl -i localhost:8099/health -H 'x-request-id: my-own-trace-id-123' | grep -i x-request-id
# x-request-id: my-own-trace-id-123
```

An abusive or oversized id is replaced rather than echoed — otherwise the header becomes a
reflected-content vector. There is a test named exactly that:
`an_abusive_request_id_is_replaced_rather_than_echoed`.

---

## API reference

### Unauthenticated

| Method | Path | Notes |
|---|---|---|
| `GET` | `/health` | Deep check — runs `SELECT 1` |
| `GET` | `/health/live` | Trivial. Never touches a dependency |
| `GET` | `/health/ready` | Deep check |

All three send `Cache-Control: no-store`.

```http
HTTP/1.1 200 OK
content-type: application/json
cache-control: no-store
x-request-id: req_019fac7203d6704380cd5fc9bb6f6dc1

{"status":"ok"}
```

When the database is unreachable, `/health` and `/health/ready` return `503` with
`{"status":"database_unreachable"}` — a body that names no internals.

**Liveness is trivial on purpose.** A liveness probe that fails on a database blip gets the
container killed, which helps nothing and drops in-flight payments.

### Authenticated (`/v1`)

| Method | Path | Scope | Notes |
|---|---|---|---|
| `POST` | `/v1/orders` | `orders:write` | |
| `GET` | `/v1/orders` | `orders:read` | Paginated |
| `GET` | `/v1/orders/{id}` | `orders:read` | |
| `POST` | `/v1/payments` | `payments:write` | The interesting one |
| `GET` | `/v1/payments/{id}` | `payments:read` | |
| `POST` | `/v1/payments/{id}/capture` | `payments:write` | Only from `authorized` |
| `POST` | `/v1/refunds` | `refunds:write` | Omit `amount` for a full refund |
| `GET` | `/v1/refunds` | `payments:read` | Requires `?payment_id=` |
| `GET` | `/v1/refunds/{id}` | `payments:read` | |
| `POST` | `/v1/webhook_endpoints` | `webhooks:manage` | Returns `signing_secret` **once** |
| `GET` | `/v1/webhook_endpoints` | `webhooks:manage` | Unpaginated, capped at 16 |
| `DELETE` | `/v1/webhook_endpoints/{id}` | `webhooks:manage` | Disables; idempotent |
| `GET` | `/v1/events` | `payments:read` | Paginated |

`/v1/events` requires `payments:read` rather than a write scope — diagnostic data should not
require a write-capable key.

---

### The payment creation flow

This is the flow the whole project exists to demonstrate.

```
auth.require(PaymentsWrite)
  │
  ├─▶ validate::cvc            shape only (3–4 digits), then DROPPED forever
  │
  ├─▶ parse OrderId            an unparseable id is a 404, never a 400
  │
  ├─▶ orders.get(merchant, id) ownership check — another merchant's id is a 404
  │
  ├─▶ simulator::decide(&pan, order.amount)
  │        ▲
  │        └── THE GATE. A card not in the published test set is a 402 and
  │            NOTHING is written to the database. This is what makes it
  │            structurally impossible for a real PAN to enter the system.
  │
  ├─▶ optional sleep: min(decision.latency_ms, config.max_simulated_latency)
  │
  ├─▶ payments.create(...)  ── ONE database transaction ──────────────────┐
  │      lock order (rank 2)                                              │
  │      insert payment row                                               │
  │      insert payment_attempt row                                       │
  │      on success: ledger postings (capture + fee), balanced or ROLLBACK │
  │      order state transition                                           │
  │      insert events + webhook_deliveries + jobs  (transactional outbox)│
  │  ─────────────────────────────────── COMMIT or nothing happened ──────┘
  │
  └─▶ status == Failed ?  402 card error WITH payment_id attached
                       :  201 with the payment object
```

A decline **commits the failed payment row first**, then reports the error. The merchant can
always fetch what happened:

```bash
curl -s -X POST localhost:8099/v1/payments -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -d '{"order_id":"...","card":{"number":"4000000000009995","exp_month":12,"exp_year":2030,"cvc":"123"}}'
```

```json
{
  "error": {
    "type": "card_error",
    "code": "insufficient_funds",
    "message": "The card has insufficient funds.",
    "param": null,
    "payment_id": "pay_019fac72-93c9-7782-9490-5e4b2052b4ad",
    "request_id": "req_019fac7293c37c839b4493f15b4c02e8"
  }
}
```

Contrast that with an unknown card, where there is no `payment_id` because **nothing was
written**:

```json
{
  "error": {
    "type": "card_error",
    "code": "card_not_in_test_set",
    "message": "This card number is not in the published test set. Only documented test cards are accepted — see /docs/testing/test-cards.",
    "param": null,
    "request_id": "req_019fac7293b5703184cf3a8001728e9a"
  }
}
```

---

### Authorize, then capture

The `4000000000000077` card authorizes without capturing — funds held, awaiting an explicit
capture call.

```bash
curl -s -X POST localhost:8099/v1/payments -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -d '{"order_id":"order_019fac73-2568-7870-8b75-4a11171331a9",
       "card":{"number":"4000000000000077","exp_month":11,"exp_year":2029,"cvc":"737"}}'
```

```json
{
  "id": "pay_019fac73-259e-75b2-91ef-d4370de4048d",
  "amount": 250000,
  "status": "authorized",
  "captured_at": null,
  "card": { "last4": "0077", "brand": "visa", "exp_month": 11, "exp_year": 2029 }
}
```

The order sits at `attempted` with `amount_paid: 0` — authorized is not paid:

```json
{ "id": "order_019fac73-2568-7870-8b75-4a11171331a9",
  "status": "attempted", "amount": 250000, "amount_paid": 0 }
```

Capture it:

```bash
curl -i -X POST localhost:8099/v1/payments/pay_019fac73-259e-75b2-91ef-d4370de4048d/capture \
  -H "Authorization: Bearer $SK" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: readme-capture-0001' -d '{}'
```

```http
HTTP/1.1 200 OK
x-request-id: req_019fac73263179608accfa4d843cd5c3

{ "id": "pay_019fac73-259e-75b2-91ef-d4370de4048d",
  "status": "captured",
  "captured_at": "2026-07-29T05:57:35.413112Z", ... }
```

Capture it twice and the state machine says no:

```json
{"error":{"type":"invalid_request_error","code":"invalid_state",
  "message":"cannot transition payment from 'Captured' to 'Captured'","param":null,
  "request_id":"req_019fac73264f7cc1b69570b7a164d174"}}
```

---

### Partial refunds

```bash
curl -s -X POST localhost:8099/v1/refunds -H "Authorization: Bearer $SK" \
  -H 'Content-Type: application/json' \
  -d '{"payment_id":"pay_019fac73-259e-75b2-91ef-d4370de4048d","amount":50000,"reason":"one item returned"}'
```

```json
{
  "id": "re_019fac73-268b-7343-bf10-7b10abfb8b35",
  "object": "refund",
  "payment_id": "pay_019fac73-259e-75b2-91ef-d4370de4048d",
  "amount": 50000,
  "currency": "INR",
  "reason": "one item returned",
  "status": "processed",
  "created_at": "2026-07-29T05:57:35.495058Z",
  "processed_at": "2026-07-29T05:57:35.495058Z"
}
```

The payment stays `captured` and tracks the running total:

```json
{ "id": "pay_019fac73-259e-75b2-91ef-d4370de4048d",
  "status": "captured", "amount": 250000, "amount_refunded": 50000 }
```

Over-refund and you get a `400` that **names the balance**, so you can correct it in one round
trip rather than guessing:

```json
{"error":{"type":"invalid_request_error","code":"parameter_invalid",
  "message":"amount: refund amount exceeds the refundable balance of 200000","param":"amount",
  "request_id":"req_019fac7326db74318aaf461e461187ee"}}
```

**Why `400` and not `409`?** A `409` says "retry and it may work". Over-refunding never becomes
valid by retrying — the refundable balance only shrinks. It is a bad parameter.

Refund the whole thing and the payment moves to `refunded`:

```json
{ "id": "pay_019fac72-dc57-76e1-985d-fed01664fc9d",
  "status": "refunded", "amount": 150000, "amount_refunded": 150000 }
```

Listing refunds is scoped to a payment, because a refund is only ever interesting relative to
one. The ownership check runs on the **payment**, so another merchant's id is a `404` rather than
an empty list — an empty list would confirm the id exists.

```bash
curl -s "localhost:8099/v1/refunds?payment_id=pay_019fac72-dc57-76e1-985d-fed01664fc9d" \
  -H "Authorization: Bearer $SK"
```
```json
{
  "object": "list",
  "data": [ { "id": "re_019fac72-dd0c-7e83-bca7-702ddbca890c", "object": "refund",
              "amount": 150000, "status": "processed", ... } ],
  "has_more": false,
  "next_before": null
}
```

---

### The events feed

Every state change writes an event row inside the same transaction as the change itself. Nothing
is ever emitted for work that rolled back.

```bash
curl -s "localhost:8099/v1/events?limit=4" -H "Authorization: Bearer $SK"
```

```json
{
  "object": "list",
  "data": [
    {
      "id": "evt_019fac73-2693-70f0-bcb3-de6e820e056a",
      "object": "event",
      "type": "refund.processed",
      "api_version": "2026-07-01",
      "created_at": "2026-07-29T05:57:35.495058Z",
      "data": {
        "refund":  { "id": "re_019fac73-268b-...", "amount": 50000, "status": "processed", ... },
        "payment": { "id": "pay_019fac73-259e-...", "amount": 250000, "amount_refunded": 50000, ... }
      }
    },
    { "id": "evt_019fac73-263f-...", "type": "payment.captured",
      "data": { "payment": {...}, "order": {...} } },
    { "id": "evt_019fac73-2640-...", "type": "order.paid",
      "data": { "order": {...} } },
    { "id": "evt_019fac73-25a2-...", "type": "payment.authorized",
      "data": { "payment": {...}, "order": {...} } }
  ],
  "has_more": true,
  "next_before": "2026-07-29T05:57:35.259475Z"
}
```

#### Event types

| Type | Fires when | `data` contains |
|---|---|---|
| `payment.authorized` | A payment authorizes without capturing | `payment`, `order` |
| `payment.captured` | Funds are captured | `payment`, `order` |
| `payment.failed` | A card is declined | `payment`, `order` |
| `order.paid` | An order reaches its full amount | `order` |
| `refund.processed` | A refund settles | `refund`, `payment` |

Payment events carry **both** the payment and the order, so a merchant's handler can find their
own record via `receipt` without a second API call.

---

## Test cards

The card table *is* the external banking world. In a real gateway this is 800 ms of ISO 8583
messages across an acquirer, a card network, and an issuing bank. Here it is a table lookup.

Two properties matter:

**Determinism.** The same card always produces the same outcome *and the same simulated latency*
(derived from the digits, roughly 300–800 ms). Real issuers are nondeterministic; a sandbox must
not be, or tutorials and graded scenarios stop being reproducible.

**Exhaustiveness as a safety control.** Any number not in this table is rejected *before any
write*. This is what makes it structurally impossible for a real card to enter the system — no
PCI scope, no live PAN in the database, no liability. It is a security boundary, not a
convenience.

All numbers below satisfy the Luhn check, so client-side validation in a checkout form behaves
exactly as it would with a real card. There is a test that asserts this
(`all_test_cards_pass_luhn`).

| Card number | Brand | Outcome | Resulting status |
|---|---|---|---|
| `4242424242424242` | Visa | Succeeds, captured immediately | `captured` |
| `4000000000000077` | Visa | Authorizes only — needs an explicit capture | `authorized` |
| `4000002760003184` | Visa | Requires 3-D Secure authentication | `requires_action` |
| `4000000000000259` | Visa | Succeeds, then a dispute opens after 60 s | `captured` |
| `5555555555554444` | Mastercard | Succeeds | `captured` |
| `6521000000000007` | RuPay | Succeeds | `captured` |
| `378282246310005` | Amex | Succeeds | `captured` |
| `4000000000000002` | Visa | Declined — generic | `402 card_declined` |
| `4000000000009995` | Visa | Declined — insufficient funds | `402 insufficient_funds` |
| `4000000000000069` | Visa | Declined — expired card | `402 expired_card` |
| `4000000000000127` | Visa | Declined — incorrect CVC | `402 incorrect_cvc` |
| `4000000000000119` | Visa | Declined — processing error | `402 processing_error` |
| `4000000000009987` | Visa | Declined — reported lost | `402 lost_card` |
| `4000000000009979` | Visa | Declined — reported stolen | `402 stolen_card` |

### The amount override

Any amount whose last two minor-unit digits are `05` goes to risk review, regardless of card.
That gives integrators a way to exercise the review path without a dedicated card, and it mirrors
how real fraud engines key partly on amount.

```bash
# ₹1500.05 on the success card
-d '{"order_id":"...","card":{"number":"4242424242424242",...}}'   # amount 150005
```
```json
{ "id": "pay_019fac75-8305-7a61-9e6c-fb00cba39a44",
  "amount": 150005, "status": "created", "captured_at": null }
```

The override wins over the card's default outcome **only for cards that would otherwise succeed**
— a declined card stays declined regardless of amount, which matches real issuer behaviour.

### Lost and stolen never leak the reason

```json
{"error":{"type":"card_error","code":"stolen_card",
  "message":"The card was declined.",
  "payment_id":"pay_019fac75-8449-7bd1-9116-be5a56e7d0ce"}}
```

The `code` says `stolen_card` for the merchant's logs, but the customer-facing `message` is
identical to a generic decline. Real gateways never tell a cardholder the card was reported
stolen — that tips off a thief. There is a test named
`lost_and_stolen_do_not_leak_the_reason_to_the_customer`.

### Brand detection

Brands are detected from the IIN prefix, not declared by the caller: `4` → Visa, `51–55` and
`2221–2720` → Mastercard, `34`/`37` → Amex, `65`/`60` → RuPay.

```json
{ "status": "captured", "card": { "brand": "amex",       "last4": "0005", ... } }
{ "status": "captured", "card": { "brand": "mastercard", "last4": "4444", ... } }
{ "status": "captured", "card": { "brand": "rupay",      "last4": "0007", ... } }
```

---

## The error envelope

There is exactly **one** error shape, for every failure, everywhere in the system. No bare
strings, no `axum` default rejections, no HTML.

```json
{
  "error": {
    "type": "invalid_request_error",
    "code": "currency_invalid",
    "message": "'XYZ' is not a supported currency.",
    "param": "currency",
    "request_id": "req_019fac72470774408cc576a78b1c6283"
  }
}
```

Optional extras: `payment_id` on card errors, and a `Retry-After` header on 503/504/429-shaped
errors.

### Error types

| `type` | Meaning |
|---|---|
| `invalid_request_error` | The request was malformed or unacceptable |
| `authentication_error` | Missing, malformed, or rejected API key |
| `card_error` | Something about the card is wrong — declines, unknown test card, bad CVC |
| `idempotency_error` | Key reuse, in-flight collision, or a uniqueness conflict |
| `api_error` | Something went wrong on our side |

### Constructors and their status codes

| Constructor | Status | Used for |
|---|---|---|
| `invalid_request(code, msg, param)` | 400 | Malformed or unacceptable input |
| `not_found(resource)` | 404 | Missing **or** not owned by this merchant |
| `unauthorized(msg)` | 401 | Missing or bad key |
| `forbidden(msg)` | 403 | Authenticated but lacking the scope |
| `unknown_endpoint(method, path)` | 404 | Router fallback |
| `method_not_allowed(method, path)` | 405 | Method fallback |
| `conflict(code, msg)` | 409 | Idempotency mismatch, in-flight, unique violations |
| `payload_too_large(limit)` | 413 | Body limit exceeded |
| `unsupported_media_type()` | 415 | Missing or wrong `Content-Type` |
| `card(code, msg, payment_id)` | 402 | Declines, unknown test card, bad CVC |
| `unavailable()` | 503 | Dependency down; sets `Retry-After` |
| `timeout()` | 504 | Request budget exceeded |
| `internal()` / `internal_from(err, ctx)` | 500 | Anything unexpected; detail goes to logs only |

### The validation catalogue, live

<details>
<summary><b>Amount below the minimum</b> — <code>400 parameter_invalid</code></summary>

```json
{"error":{"type":"invalid_request_error","code":"parameter_invalid",
  "message":"amount: amount must be at least 100 minor units","param":"amount",
  "request_id":"req_019fac7246b67bf3a9eed0f4103f1822"}}
```
This rule lives in `engine`, not `api` — it holds no matter which transport called in.
</details>

<details>
<summary><b>Unsupported currency</b> — <code>400 currency_invalid</code></summary>

```json
{"error":{"type":"invalid_request_error","code":"currency_invalid",
  "message":"'XYZ' is not a supported currency.","param":"currency",
  "request_id":"req_019fac72470774408cc576a78b1c6283"}}
```
Supported: `INR`, `USD`, `EUR`, `JPY`, `KWD` — deliberately including a zero-decimal currency
(JPY) and a three-decimal one (KWD), so minor-unit handling is actually exercised.
</details>

<details>
<summary><b>A typo'd field</b> — <code>400</code>, named, never silently ignored</summary>

```json
{"error":{"type":"invalid_request_error","code":"parameter_invalid",
  "message":"ammount: unknown field `ammount`, expected one of `amount`, `currency`, `receipt`, `notes` at line 1 column 42",
  "param":"ammount","request_id":"req_019fac72473c7751bd91341847d5abc2"}}
```
Every request DTO derives `#[serde(deny_unknown_fields)]`. On `POST /v1/refunds` this is what
turns `"ammount": 500` into a `400` instead of an accidental **full refund**.
</details>

<details>
<summary><b>A nested field</b> — <code>param</code> names the full path</summary>

```json
{"error":{"type":"invalid_request_error","code":"parameter_invalid",
  "message":"card.name: unknown field `name`, expected one of `number`, `exp_month`, `exp_year`, `cvc` at line 1 column 142",
  "param":"card.name","request_id":"req_019fac72931b7c33a68ad9b0b885eeda"}}
```
</details>

<details>
<summary><b>Malformed JSON</b> — <code>400 json_invalid</code></summary>

```json
{"error":{"type":"invalid_request_error","code":"json_invalid",
  "message":"Request body is not valid JSON.","param":null,
  "request_id":"req_019fac7247717b22b7f37113c23dc65f"}}
```
Note it does **not** echo the body. Serde's raw parse error can contain fragments of the
payload — which, on `/v1/payments`, is a card number.
</details>

<details>
<summary><b>Missing Content-Type</b> — <code>415</code></summary>

```json
{"error":{"type":"invalid_request_error","code":"unsupported_media_type",
  "message":"Send 'Content-Type: application/json'.","param":null,
  "request_id":"req_019fac7247a77b41982d29fe55ad7436"}}
```
</details>

<details>
<summary><b>Body over 256 KiB</b> — <code>413</code>, refused before buffering</summary>

```json
{"error":{"type":"invalid_request_error","code":"request_too_large",
  "message":"Request body is too large.","param":null,
  "request_id":"req_019fac747cb570c3b6a90f169530e216"}}
```
</details>

<details>
<summary><b>Malformed CVC</b> — <code>402</code>, and it never quotes the value</summary>

```json
{"error":{"type":"card_error","code":"incorrect_cvc",
  "message":"The security code must be 3 or 4 digits.","param":null,
  "request_id":"req_019fac7293e17a138ff7a2ffb96ca265"}}
```
A `402` rather than a `400`, because the *card data* is what is wrong. And the offending value is
never echoed — a CVC must not appear in a response body any more than in a log line. Test:
`a_malformed_cvc_is_a_card_error_that_never_quotes_the_value`.
</details>

<details>
<summary><b>A duplicate receipt</b> — <code>409</code>, not <code>500</code></summary>

```json
{"error":{"type":"idempotency_error","code":"resource_already_exists",
  "message":"That resource already exists.","param":null,
  "request_id":"req_019fac7246a870b1a0505e6a4f12071d"}}
```
This one exists because of a real bug. The unique index `orders_merchant_receipt_idx` was firing,
and a catch-all `map_err(|_| ApiError::internal())` was swallowing it into a `500`. The fix was
the `EngineError::Store(s) => ApiError::from(s)` arm, which preserves the mapping. The regression
test is named `a_duplicate_receipt_is_a_409_not_a_500`.

**This is the anti-pattern to avoid in this codebase:**
```rust
// WRONG — throws away a mapping that already exists and turns a
// fixable 409 into an unfixable 500.
something().await.map_err(|_| ApiError::internal())?;
```
</details>

<details>
<summary><b>An unknown or unowned id</b> — <code>404</code>, never <code>403</code></summary>

```json
{"error":{"type":"invalid_request_error","code":"resource_missing",
  "message":"No such order.","param":null,
  "request_id":"req_019fac7294187cf28c07ba648bff1bf0"}}
```
Ownership failures must be **indistinguishable** from "does not exist", or ids become probeable.
An unparseable id is also a `404` — `.parse().map_err(|_| ApiError::not_found("order"))?` — not a
`400`, for the same reason.
</details>

---

## Authentication

```
Authorization: Bearer sk_test_...
```

Only **secret test keys** are valid on the server API. The other three shapes each get a distinct,
educational rejection.

<details open>
<summary><b>No key at all</b></summary>

```http
HTTP/1.1 401 Unauthorized
www-authenticate: Bearer realm="payment-sandbox"

{"error":{"type":"authentication_error","code":"invalid_api_key",
  "message":"Missing API key. Send 'Authorization: Bearer sk_test_...'.","param":null,
  "request_id":"req_019fac7203ed783299a5a62225e4acdb"}}
```
</details>

<details open>
<summary><b>A publishable key</b> — the mistake everyone makes once</summary>

```json
{"error":{"type":"authentication_error","code":"invalid_api_key",
  "message":"Publishable keys cannot call the server API. Use your secret key (sk_test_...) — and never expose it in browser code.",
  "param":null,"request_id":"req_019fac7203f77ef28cf3ede27d2a43ac"}}
```
</details>

<details open>
<summary><b>A live key</b> — rejected outright</summary>

```json
{"error":{"type":"authentication_error","code":"invalid_api_key",
  "message":"This sandbox only accepts test keys (sk_test_...). Never send a live key here.",
  "param":null,"request_id":"req_019fac72045a73d2a60105c80f5205f2"}}
```
</details>

401s carry a `WWW-Authenticate: Bearer realm="payment-sandbox"` challenge, with
`error="invalid_token"` or `error="insufficient_scope"` where those apply.

### Scopes

`ScopeSet` is a `u16` bitset over six scopes. A secret key gets all of them by default; narrower
keys are an explicit opt-in, not the default. A publishable key gets **none** — it authenticates
checkout sessions through a separate, more restricted path, never through this scope system.

| Scope | Grants |
|---|---|
| `orders:read` | `GET /v1/orders`, `GET /v1/orders/{id}` |
| `orders:write` | `POST /v1/orders` |
| `payments:read` | `GET /v1/payments/{id}`, `/v1/events`, `/v1/refunds*` |
| `payments:write` | `POST /v1/payments`, `POST /v1/payments/{id}/capture` |
| `refunds:write` | `POST /v1/refunds` |
| `webhooks:manage` | All of `/v1/webhook_endpoints` |

Every handler's first line is `auth.require(Scope::X)?;`. Failing it is a `403`, never a silent
no-op.

### The auth cache

`AuthCache` memoises key lookups so a burst of requests does not run Argon2id every time. Defaults:
TTL 5 s, capacity 4096, `last_used_at` touch interval 60 s.

It uses a **monotonic `Instant`**, not wall-clock time, so an NTP correction or a manual clock
change cannot extend a cache entry's life. **Any handler that revokes a key must call
`AuthCache::invalidate_key`.** `AuthCache::disabled()` exists for tests.

---

## Idempotency

```
Idempotency-Key: <8–255 printable ASCII>
```

Sent on mutating requests (POST/PUT/PATCH/DELETE). Reads pass straight through.

**Requests without the header also pass through.** That is a deliberate availability choice for a
sandbox that must stay usable from a bare `curl`. A production deployment would make the header
required on `/v1/payments`.

The whole protocol is decided by **one atomic `INSERT ... ON CONFLICT DO NOTHING`** in
`store::idempotency`. There is no read-then-write window for two concurrent retries to slip
through.

| Outcome | Meaning | Response |
|---|---|---|
| `Acquired` | First time seeing this key | Execute, then record the response |
| `Replay` | Completed, **same** request fingerprint | The stored response verbatim + `idempotent-replayed: true` |
| `Mismatch` | Completed, **different** fingerprint | `409` — a client bug; never be lenient here |
| `InFlight` | Still executing | `409 idempotency_key_in_flight` |

### Replay, live

The same key with the same body returns the **original** response — same order id, same
`created_at`, seventeen seconds later:

```http
HTTP/1.1 201 Created
idempotent-replayed: true
x-request-id: req_019fac72468571428d970455a8355e88

{"id":"order_019fac72-0499-7602-a486-a4d7e88780a2",
 "created_at":"2026-07-29T05:56:21.273122Z","amount":150000,"status":"created", ...}
```

Note the `x-request-id` is **new** — this is a distinct request that happened to replay a stored
response — while the body is byte-identical to the original.

### Mismatch

The same key with a *different* body is a `409`, always:

```json
{"error":{"type":"idempotency_error","code":"idempotency_key_reused",
  "message":"This Idempotency-Key was already used for a request with a different body. Use a new key for a new request.",
  "param":null,"request_id":"req_019fac7246997c12b531738d2c203cda"}}
```

### Design decisions worth knowing

- **The fingerprint covers method + path + body, deliberately *not* headers.** A proxy that adds a
  trace header must not turn a legitimate retry into a `409`.
- **Keys are scoped per merchant.** Two merchants can use `order-1` without colliding. Test:
  `idempotency_keys_are_scoped_per_merchant`.
- **Store unreachable → `503`.** Never proceed unprotected. Failing closed on a payments endpoint
  is the only safe direction.
- **A handler returning 5xx releases the key**, so a retry re-executes rather than replaying a
  server error forever.
- **A declined payment replays as the same decline.** A `402` is a legitimate, recorded outcome —
  retrying with the same key must not re-run the card. Test:
  `a_declined_payment_replays_as_the_same_decline`.
- Responses larger than `max_recorded_response_bytes` are returned but not recorded, and say so in
  the log.

---

## Pagination

Cursor-based and uniform across every collection.

```
?limit=<1..=100>&before=<RFC3339>
```

```bash
curl -s "localhost:8099/v1/orders?limit=2" -H "Authorization: Bearer $SK"
```
```json
{
  "object": "list",
  "data": [ {...}, {...} ],
  "has_more": true,
  "next_before": "2026-07-29T05:57:35.208932Z"
}
```

Feed `next_before` back to get the following page:

```bash
curl -s "localhost:8099/v1/orders?limit=2&before=2026-07-29T05:57:35.208932Z" \
  -H "Authorization: Bearer $SK"
```

Internally `fetch_limit()` requests `limit + 1` rows; the extra row's existence is what answers
`has_more`, with no second `COUNT(*)` query.

### An out-of-range limit is rejected, not clamped

```json
{"error":{"type":"invalid_request_error","code":"limit_invalid",
  "message":"'limit' must be between 1 and 100, got 500.","param":"limit",
  "request_id":"req_019fac747bc776829b0511681a994374"}}
```

A caller paginating on `limit=500` who silently receives 100 items with no explanation will
conclude they have reached the end of the collection. Rejecting with a message naming the maximum
is the only honest answer. Test:
`an_out_of_range_limit_is_an_error_rather_than_a_silent_clamp`.

`ListParams` also uses `deny_unknown_fields`, so a typo in the query string is an error rather
than a silently-defaulted page:

```json
{"error":{"type":"invalid_request_error","code":"parameter_invalid",
  "message":"Invalid query string. Failed to deserialize query string: limitt: unknown field `limitt`, expected `limit` or `before`",
  "param":null,"request_id":"req_019fac747bff7c53b53f8c6cab382fd7"}}
```

Defaults: `default_page_size` 10, `max_page_size` 100. `engine::order::MAX_LIST_LIMIT = 200`
clamps at the engine boundary as a second line of defence.

---

## Webhooks

### Registering an endpoint

```bash
curl -i -X POST localhost:8099/v1/webhook_endpoints \
  -H "Authorization: Bearer $SK" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: readme-wh-0002' \
  -d '{"url":"https://merchant.example.com/hooks/sandbox",
       "enabled_events":["payment.captured","payment.failed","refund.processed","order.paid"]}'
```

```http
HTTP/1.1 201 Created
x-request-id: req_019fac73a50473b39d39421f3023489d

{
  "id": "we_019fac73-a509-7412-96c1-ebd429c465e0",
  "object": "webhook_endpoint",
  "url": "https://merchant.example.com/hooks/sandbox",
  "enabled_events": ["payment.captured","payment.failed","refund.processed","order.paid"],
  "signing_secret": "whsec_84f1c28e...REDACTED...94bf9d23",
  "created_at": "2026-07-29T05:58:07.881237Z"
}
```

**That is the only time `signing_secret` is ever emitted.** Every other serialisation path goes
through a private `render()` that structurally cannot produce it — the field is set by hand at
exactly one call site. Listing proves it:

```json
{
  "object": "list",
  "data": [ { "id": "we_019fac73-a509-7412-96c1-ebd429c465e0",
              "object": "webhook_endpoint",
              "url": "https://merchant.example.com/hooks/sandbox",
              "enabled_events": [...],
              "created_at": "2026-07-29T05:58:07.881237Z" } ],
  "has_more": false
}
```

Test: `webhook_endpoint_secret_shown_once`.

**`MAX_ACTIVE_ENDPOINTS = 16` per merchant** — a blast-radius control, since every event fans out
to every active endpoint. The cap is checked *before* a secret is minted, so a rejected request
leaves no trace.

A bad URL is refused (parsed with `url::Url`, max 2048 chars):

```json
{"error":{"type":"invalid_request_error","code":"url_invalid",
  "message":"Webhook URL is not a valid absolute URL, e.g. https://example.com/hooks.",
  "param":"url","request_id":"req_019fac7366cd7903b558da3f9703e2ad"}}
```

`DELETE` **disables** rather than deletes, because undelivered outbox jobs still reference the
row. It is idempotent — disabling an already-disabled endpoint is a no-op success:

```http
HTTP/1.1 200 OK
{"id":"we_019fac73-a509-7412-96c1-ebd429c465e0","object":"webhook_endpoint","deleted":true}

# exactly the same call again → HTTP 200
```

### A real delivery

Registering a local receiver on `127.0.0.1:9310` and taking one Mastercard payment produced these
two deliveries, captured verbatim:

```text
POST /hooks
Content-Type: application/json
X-Sandbox-Signature: t=1785304721,v1=6c28326fa556db9ee7ca8e035c0aa1155426a63136933704f7b04f51aa735473
X-Sandbox-Event-Id: evt_019fac74-2787-7620-81e3-787e7bd154fc
X-Sandbox-Event-Type: payment.captured
User-Agent: payment-sandbox-webhooks/0.1

{
  "id": "evt_019fac74-2787-7620-81e3-787e7bd154fc",
  "type": "payment.captured",
  "api_version": "2026-07-01",
  "created_at": "2026-07-29T05:58:41.275973Z",
  "data": {
    "order": {
      "id": "order_019fac74-275a-7a60-8198-3fba63fc07d0",
      "amount": 99900, "amount_paid": 99900, "currency": "INR",
      "receipt": "rcpt_readme_hook", "status": "paid", "notes": {},
      "created_at": "2026-07-29T05:58:41.242649Z"
    },
    "payment": {
      "id": "pay_019fac74-277e-7580-9647-9684a50c5086",
      "amount": 99900, "amount_refunded": 0, "currency": "INR",
      "status": "captured", "method": "card",
      "card": { "last4": "4444", "brand": "mastercard", "exp_month": 10, "exp_year": 2031 },
      "captured_at": "2026-07-29T05:58:41.275973Z"
    }
  }
}
```

And in the server log:

```text
INFO webhooks::worker: webhook delivered event=order.paid       endpoint=http://127.0.0.1:9310/hooks status=200 attempt=1
INFO webhooks::worker: webhook delivered event=payment.captured endpoint=http://127.0.0.1:9310/hooks status=200 attempt=1
```

### Verifying the signature

```
X-Sandbox-Signature: t=1785304721,v1=6c28326f...
```

We sign `"{timestamp}.{raw_body}"` with HMAC-SHA256, not the body alone, and send both. Without
the timestamp, an attacker who captured one valid webhook could replay it forever. Consumers must
reject any timestamp outside a tolerance window — **default 300 seconds**.

```python
import hmac, hashlib, time

def verify(secret: str, header: str, raw_body: bytes, tolerance: int = 300) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    ts, provided = int(parts["t"]), parts["v1"]

    if abs(int(time.time()) - ts) > tolerance:
        return False                                  # replay window exceeded

    expected = hmac.new(
        secret.encode(), f"{ts}.".encode() + raw_body, hashlib.sha256
    ).hexdigest()

    # CONSTANT TIME. Never use `==` here.
    return hmac.compare_digest(expected, provided)
```

```javascript
const crypto = require("crypto");

function verify(secret, header, rawBody, tolerance = 300) {
  const parts = Object.fromEntries(header.split(",").map(p => p.split("=")));
  const ts = parseInt(parts.t, 10);
  if (Math.abs(Math.floor(Date.now() / 1000) - ts) > tolerance) return false;

  const expected = crypto
    .createHmac("sha256", secret)
    .update(`${ts}.`)
    .update(rawBody)          // the RAW bytes, not a re-serialised object
    .digest("hex");

  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(parts.v1));
}
```

Two things integrators get wrong, both of which this sandbox will let you discover safely:

1. **Comparing with `==`.** String equality short-circuits on the first differing byte, leaking
   through timing how many leading bytes an attacker guessed correctly — enough to derive a valid
   signature byte by byte. Our own `crypto::signing::verify` uses `mac.verify_slice`, which is
   constant-time, and the source says so in a comment.
2. **Re-serialising the JSON before hashing.** Sign the **raw bytes you received**. Any parse and
   re-emit reorders keys or changes whitespace, and the signature will never match.

The header parser ignores unknown `vN=` keys, so a future `v2` scheme can roll out without
breaking existing consumers.

### The transactional outbox

There is no window where a payment commits but its webhook is lost, and none where a webhook
fires for a payment that rolled back.

```
BEGIN
  lock order
  insert payment
  insert payment_attempt
  insert ledger_transaction + ledger_entries     ← trigger verifies balance at COMMIT
  update order status
  insert events                                  ┐
  insert webhook_deliveries (one per endpoint)   ├─ the outbox
  insert jobs               (one per delivery)   ┘
COMMIT
```

Only after `COMMIT` does the background worker see the job. If the transaction rolls back, every
trace of the event vanishes with it.

The `events.payload` column stores only the `data` object; the delivery worker assembles the full
envelope (`{id, type, created_at, api_version, data}`) at send time. That way the envelope shape
is defined in exactly one place.

### The retry ladder

The ladder is an explicit table, not a formula, because the exact intervals matter to the learning
experience — a merchant watching the delivery log should see recognizable spacing.

| Attempt fails | Next retry |
|---|---|
| 1 | 1 minute |
| 2 | 5 minutes |
| 3 | 30 minutes |
| 4 | 2 hours |
| 5 | 6 hours |
| 6 | 24 hours |
| 7 | **dead** — no further retries |

Delivery timeout is 10 seconds, covering connect, send, and reading the response. Non-2xx and
transport errors both count as failures. Response bodies are truncated before being stored.

---

## The double-entry ledger

Every capture and every refund posts a balanced accounting transaction. This is not decoration —
it is the part that makes the sandbox worth studying, because it is where a real gateway's
correctness actually lives.

### The chart of accounts

| Account | Normal balance | Scope | Meaning |
|---|---|---|---|
| `gateway_clearing` | Debit | Platform | Funds in flight from the card network — our claim on the acquiring bank |
| `merchant_pending` | Credit | Merchant | Owed to the merchant, not yet through the settlement window |
| `merchant_available` | Credit | Merchant | Cleared and payable |
| `merchant_reserve` | Credit | Merchant | Withheld as a risk reserve |
| `dispute_holding` | Debit | Merchant | Frozen while a dispute is open |
| `platform_revenue` | Credit | Platform | Our fee income |
| `tax_payable` | Credit | Platform | GST collected on fees, owed to the tax authority |

Sign convention throughout the codebase: **a positive entry amount is a credit, a negative amount
is a debit, and a transaction is valid only when its entries sum to exactly zero.**

`AccountType::normal_balance()` is an exhaustive match with no wildcard, so adding a new account
type will not compile until you classify it. That compile error is the point.

### What one capture actually posts

A ₹999.00 capture, straight out of the database:

```sql
SELECT lt.kind, la.account_type, le.amount_minor
FROM ledger_entries le
JOIN ledger_transactions lt ON lt.id = le.transaction_id
JOIN ledger_accounts     la ON la.id = le.account_id
WHERE lt.reference_id = '019fac74-277e-7580-9647-9684a50c5086'
ORDER BY lt.created_at, le.amount_minor DESC;
```

```text
       kind       |   account_type   | amount_minor
------------------+------------------+--------------
 payment_captured | merchant_pending |        99900     credit — we owe the merchant
 payment_captured | gateway_clearing |       -99900     debit  — the network owes us
 fee_charged      | platform_revenue |         2298     credit — our revenue
 fee_charged      | tax_payable      |          414     credit — GST we owe
 fee_charged      | merchant_pending |        -2712     debit  — deducted from the merchant
(5 rows)
```

Two separate transactions — the capture and the fee — each balancing to zero on its own. The
merchant is owed `99900 - 2712 = 97188` paise.

### The invariant, proven

```sql
SELECT COUNT(*) AS transactions, SUM(total) AS grand_total_must_be_zero
FROM (SELECT transaction_id, SUM(amount_minor) AS total
      FROM ledger_entries GROUP BY transaction_id) t;
```

```text
 transactions | grand_total_must_be_zero
--------------+--------------------------
           14 |                        0
```

Current balances across every account:

```text
   account_type   | currency | balance_minor
------------------+----------+---------------
 gateway_clearing | INR      |       -599900
 merchant_pending | INR      |        584397
 platform_revenue | INR      |         13138
 tax_payable      | INR      |          2365
```

`-599900 + 584397 + 13138 + 2365 = 0`. The books balance.

### Three layers of enforcement

An unbalanced posting is not merely discouraged; it is unrepresentable, and then it is rejected
twice more:

1. **In the type system.** Every posting rule builds a `BalancedTransaction`. There is no
   constructor that produces an unbalanced one.
2. **In the database, at COMMIT.** A `DEFERRABLE INITIALLY DEFERRED` trigger re-verifies:

   ```text
   Triggers:
       ledger_entries_balance_check AFTER INSERT ON ledger_entries
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
         EXECUTE FUNCTION ledger_transaction_must_balance()
   ```

   Deferred matters: entries are inserted one at a time, so the transaction is legitimately
   unbalanced mid-flight. The check has to run at `COMMIT`, not per row.
3. **By making history immutable.**

   ```text
       ledger_entries_no_delete BEFORE DELETE ON ledger_entries
         FOR EACH ROW EXECUTE FUNCTION ledger_entries_forbid_mutation()
       ledger_entries_no_update BEFORE UPDATE ON ledger_entries
         FOR EACH ROW EXECUTE FUNCTION ledger_entries_forbid_mutation()
   ```

   Plus `CHECK (amount_minor <> 0)` and `CHECK (currency ~ '^[A-Z]{3}$')`. A mistake is corrected
   by posting a reversing entry, exactly as in real accounting — never by editing the past.

### Lock ordering

Deadlocks in a payments system are not theoretical, so lock order is documented and ranked in
`store::tx::LockOrder`:

| Operation | Locks |
|---|---|
| `payment.create` | ORDER (rank 2) only — the payment does not exist yet |
| `payment.capture` | PAYMENT (rank 1), then ORDER (rank 2) |
| `refund.create` | PAYMENT (rank 1) only |

Refunds deliberately do **not** lock the order — a refund does not change it; the order stays
`paid`. Taking a lock you do not need widens the deadlock surface for nothing.

Ledger accounts are never locked. Entries are inserts and balances are sums, so there is nothing
to contend on.

---

## Fees

All arithmetic in integer minor units, rounding **half-up** via `Money::percent`. The default
schedule mirrors Indian gateway pricing.

| Currency | Percentage | Fixed | Tax on the fee |
|---|---|---|---|
| `INR` | 2% (200 bps) | ₹3.00 (300 paise) | 18% GST |
| everything else | 2% (200 bps) | 30 minor units | none |

Percentages are stored in **basis points**, so a schedule like 2.9% is representable without ever
touching a float.

Worked example on ₹1,500.00:

```
150000 × 2%          = 3000
       + 300 fixed   = 3300   base      → platform_revenue
3300   × 18% GST     =  594   tax       → tax_payable
                       ────
                       3894   total     → debited from merchant_pending
```

### Why refund fee reversal is computed cumulatively

The obvious implementation — `fee_total × this_refund ÷ payment_amount` — drifts. Refund a ₹1,500
payment in three ₹500 slices and rounding can strand a paisa of fee on the platform's books
forever.

Instead we compute what the **cumulative** fee owed should be at the new refunded total, and
subtract what was already owed at the old total. When the payment is fully refunded the cumulative
figure is exactly `fee_total`, so the final slice always returns the exact remainder.
Self-correcting by construction.

### A known, documented edge

The minimum order is 100 minor units, and the INR fee on it is 356 — larger than the amount. That
is allowed: the merchant's pending balance goes negative, meaning they owe the platform. Real
gateways handle this with per-currency minimums. We document it and keep a test named
`fee_can_exceed_tiny_amounts`, rather than pretending it cannot happen.

---

## Data model

Postgres 16 on **port 54432**. 21 migration up/down pairs in [migrations/](migrations/), applied
automatically at boot.

```text
 merchants ──┬── api_keys                 Argon2id hashes, scopes, revocation
             │
             ├── orders ──── payments ──┬── payment_attempts   one row per try, with latency
             │                          ├── payment_methods    last4 + brand + fingerprint ONLY
             │                          ├── refunds            full and partial
             │                          └── disputes           evidence deadlines  (v2)
             │
             ├── ledger_accounts ── ledger_transactions ── ledger_entries
             │                                             ↑ deferred balance trigger
             │                                             ↑ immutable: no UPDATE, no DELETE
             │
             ├── events ──── webhook_deliveries ──── webhook_endpoints
             │      ↑              ↑
             │      └── outbox ────┴──── jobs        the worker's queue
             │
             ├── settlements        the T+2 sweep                          (v2)
             ├── audit_logs         who did what, when
             └── idempotency_keys   24h TTL, per-merchant scope
```

### Ids are typed and prefixed

Every id is a UUIDv7 with a human-readable prefix, so an id in a log line or a bug report tells
you what it is without a lookup. UUIDv7 is time-ordered, which keeps B-tree inserts sequential and
makes `created_at` cursors behave.

| Prefix | Resource |
|---|---|
| `mer_` | Merchant |
| `key_` | API key |
| `order_` | Order |
| `pay_` | Payment |
| `re_` | Refund |
| `evt_` | Event |
| `we_` | Webhook endpoint |
| `whsec_` | Webhook signing secret |
| `sk_test_` / `pk_test_` | Secret / publishable API key |

They are distinct Rust types too — `OrderId` and `PaymentId` are not interchangeable, so passing
one where the other belongs is a compile error rather than a `404` in production.

### Cardholder data, in full

This is the entire `payment_methods` table. There is no column for a PAN and none for a CVC:

```text
                               Table "public.payment_methods"
   Column    |           Type           | Nullable |          Default
-------------+--------------------------+----------+-----------------------------
 id          | uuid                     | not null |
 merchant_id | uuid                     | not null |
 type        | payment_method_type      | not null | 'card'::payment_method_type
 last4       | character(4)             | not null |
 brand       | card_brand               | not null |
 exp_month   | smallint                 | not null |
 exp_year    | smallint                 | not null |
 fingerprint | text                     | not null |
 created_at  | timestamp with time zone | not null |
Check constraints:
    "pm_last4_shape" CHECK (last4 ~ '^[0-9]{4}$')
```

The `fingerprint` is a **peppered** hash. The pepper lives in `CRYPTO_CARD_FINGERPRINT_PEPPER`,
never in the database, so a database dump alone cannot be brute-forced against the 14-entry card
table. It exists so the same card can be recognised across payments — for velocity checks —
without storing anything that can be turned back into a card number.

---

## Security model

Ten constraints. These are not guidelines; several are enforced by the compiler or by CI, and the
rest have tests named after them.

| # | Constraint | How it is enforced |
|---|---|---|
| 1 | **Postgres runs on 54432, never 5432** | `compose.yaml`; every command in this repo |
| 2 | **A PAN never reaches a log line** | Hand-written `Debug` impls that render `[redacted]`; test `debug_output_never_contains_the_pan_or_the_cvc` |
| 3 | **A CVC is never stored** | Shape-checked at the edge, then dropped. No struct in `engine` or `store` has the field |
| 4 | **A non-test card never touches the database** | The simulator gate runs before any write; test `unknown_card_is_rejected_with_no_payment_created` |
| 5 | **Signing secrets are shown exactly once** | Every other path goes through a `render()` that cannot emit it; test `webhook_endpoint_secret_shown_once` |
| 6 | **Internal error detail never reaches the wire** | Store/ledger/engine internals map to a generic 500; detail goes to `tracing` |
| 7 | **Every error body uses the one envelope** | Fallbacks + extractor wrappers + `CatchPanicLayer` |
| 8 | **Cross-merchant access is a 404, never a 403** | Ownership failures are indistinguishable from "does not exist" |
| 9 | **No `unwrap`/`expect`/`todo!` in non-test code** | CI denies `clippy::unwrap_used`, `expect_used`, `todo` |
| 10 | **`unsafe` is forbidden** | `#![forbid(unsafe_code)]` in every crate |

### Additional hardening

- **`Authorization` and `Cookie` are marked sensitive** *before* `TraceLayer` reads them, which is
  what keeps API keys out of the logs. The layer order is what makes this work — outside tracing,
  not inside.
- **API keys are hashed with Argon2id.** The plaintext exists for the duration of one `println!`.
- **The auth cache uses a monotonic clock**, so a wall-clock change cannot extend an entry's life.
- **Panics are caught and rendered as a generic 500.** The payload detail is logged; the response
  leaks nothing.
- **Malformed JSON errors do not echo the body**, because on `/v1/payments` the body is a card
  number.
- **`float_arithmetic` is denied workspace-wide.** Money is integer minor units. There is no
  floating-point arithmetic anywhere in this codebase, and there must not be.

### The redacted Debug

`CardRequest` has a hand-written `Debug` that renders both `number` and `cvc` as `[redacted]` —
not even last-four, because the type exists *before* validation, so the field may hold anything at
all. **Never derive `Debug` on a type holding a PAN.**

---

## Configuration

### Server environment

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | — (required) | `postgres://sandbox:sandbox@localhost:54432/sandbox_dev` |
| `SERVER_HOST` | `0.0.0.0` | Bind address |
| `SERVER_PORT` | `8080` | Bind port |
| `CRYPTO_CARD_FINGERPRINT_PEPPER` | — (required) | Server-side pepper for card fingerprints |
| `CRYPTO_MASTER_KEY` | — (required) | KEK for envelope encryption (validated now, used by the vault work) |
| `SIMULATE_LATENCY` | `true` | Sleep for the simulator's decided latency before responding |
| `RUST_LOG` | `info` | e.g. `info,sandbox_server=debug` |

Generate real crypto values with `openssl rand -hex 32`. Leaving the example values in place logs
a loud warning at boot — fine for local dev, never for a shared deployment.

`.env` is loaded as a dev convenience; real deployments set real environment variables.

### API limits — `ApiConfig`

Every number a request can push against lives in [crates/api/config.rs](crates/api/config.rs)
rather than being spelled inline at whichever call site happens to enforce it. An operator tuning
a deployment has one file to read, and a test can shrink a limit to prove the enforcement path
works without actually sending a megabyte.

| Field | Default | Rationale |
|---|---|---|
| `max_body_bytes` | 256 KiB | Enormous headroom over a ~500-byte payment request, still cheap per connection |
| `max_recorded_response_bytes` | 256 KiB | Bigger responses are returned but not replayable |
| `request_timeout` | 30 s | Past this, the handler future is dropped, releasing its pool slot |
| `idempotency_ttl` | 24 h | Industry norm; matches the purge job |
| `default_page_size` | 10 | |
| `max_page_size` | 100 | Rejected above this, not clamped |
| `simulate_latency` | `true` | `false` in tests |
| `max_simulated_latency` | 5 s | Keeps "realistic timing" from becoming "connection held open" |

These are **defence in depth, not business rules.** Business rules — minimum order amount, allowed
currencies, note limits — live in `engine`, where they hold no matter which transport called in.

A unit test asserts the defaults are internally consistent: `default_page_size <= max_page_size`
and `max_simulated_latency < request_timeout`.

### Where each validation lives

| Layer | Owns | Examples |
|---|---|---|
| `domain` | What is true of a value forever | Luhn validity, non-negative amounts, currency codes |
| `engine` | Business rules | Minimum order amount, note limits, refund balance |
| `api/validate.rs` | Only what is meaningful *because it arrived over HTTP* | `cvc` shape, `webhook_url`, `enabled_events`, `idempotency_key` |

**Never re-check in `api` something `engine` already guarantees.** If the message is bad, improve
the engine's message. A second copy of a rule is a rule that will drift.

---

## Development

```bash
export DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev

cargo build --workspace
cargo test  --workspace          # integration tests need the DB up
cargo fmt --all

cargo clippy --all-targets --all-features -- \
  -D warnings -D clippy::unwrap_used -D clippy::expect_used -D clippy::todo \
  -D clippy::float_arithmetic -D clippy::cast_possible_truncation -D clippy::cast_sign_loss
```

That clippy line is **exactly** what [.github/workflows/ci.yaml](.github/workflows/ci.yaml) runs.
Match it locally before calling a change clean.

`clippy.toml` sets `allow-unwrap-in-tests` and `allow-expect-in-tests`, but those only apply inside
`#[test]` functions and `#[cfg(test)]` modules. Module-level helpers in an integration-test binary
are **not** covered, which is why `crates/api/tests/http_api.rs` carries a file-scoped
`#![allow(...)]` with a comment explaining why. Do not loosen the workspace lints to fix this.

### sqlx offline mode

Queries are verified at compile time against the checked-in [.sqlx/](.sqlx/) directory, so CI
compiles without a database. **If you change any `sqlx::query!` or `query_scalar!` string, you
must regenerate:**

```bash
DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev cargo sqlx prepare --workspace
```

This is why `health.rs` keeps the exact literal `"SELECT 1 AS one"` — changing the text
invalidates the cached entry.

### Adding a new resource

1. Create `crates/api/routes/<resource>.rs`.
2. Open it with a doc comment saying **why** this module exists and what is special about it.
3. Export `pub fn routes() -> Router<Arc<AppState>>` returning only routes — **no layers**. Layers
   are composed once in `routes/mod.rs`.
4. Handlers take `State(state)`, `Extension(auth)`, and `crate::extract::{Json, Query, Path}` —
   never `axum::extract::Json` for request bodies.
5. First line of every handler: `auth.require(Scope::X)?;`
6. Request DTOs derive `Deserialize` with `#[serde(deny_unknown_fields)]`.
7. Parse ids with `.parse().map_err(|_| ApiError::not_found("<resource>"))?`.
8. Serialise through a private `fn render(&T) -> Value`, so there is one projection per resource.
9. Register with `pub mod <resource>;` and `.merge(<resource>::routes())` in `routes/mod.rs`.
10. Add unit tests in the same file and black-box tests in `tests/http_api.rs`.

### House style

Match this or the code will look foreign:

- **Module doc comments explain *why*, not *what*.** Several paragraphs on the problem solved, the
  alternative considered, and what breaks if you do it the other way. Read
  [middleware/idempotency.rs](crates/api/middleware/idempotency.rs) or
  [engine/refund.rs](crates/engine/refund.rs) for the register.
- **Inline comments record decisions**, especially where a reader would ask "why not the obvious
  thing?"
- **Tests are named as sentences.** `a_duplicate_receipt_is_a_409_not_a_500`,
  `no_response_ever_echoes_a_full_card_number`,
  `liveness_answers_without_a_database_and_forbids_caching`. Each test proves one behaviour and its
  name states that behaviour.
- **Unit tests live in `#[cfg(test)] mod tests`** at the bottom of the file they test. Black-box
  HTTP tests go in `crates/api/tests/http_api.rs`.
- **Constants are named and documented**, never spelled inline at a call site.
- `#[must_use]` on pure constructors and accessors.
- `thiserror` per crate; `anyhow` only in `sandbox-server`.
- Prefer `Duration` and `OffsetDateTime` over integer seconds in signatures.
- **When something is deliberately not done, say so in a comment with the reason.**

---

## Testing

398 tests, all green.

```text
crate            unit    http    notes
─────────────────────────────────────────────────────────────────────
api               114      36    36 black-box tests over the real router
domain             88       —    invariants, state machines, money
store              66       —    against a live Postgres
crypto             40       —    signing, fingerprints, key generation
engine             24       —    orchestration and posting rules
simulator          18       —    the decision table
queue               9       —    the retry ladder
webhooks            3       —    delivery and signature
─────────────────────────────────────────────────────────────────────
                  362      36    = 398 passing (plus 2 ignored doctests)
```

```bash
cargo test --workspace
```

```text
test result: ok. 114 passed; 0 failed; 0 ignored    api   (unit)
test result: ok.  36 passed; 0 failed; 0 ignored    api   (http_api)
test result: ok.  40 passed; 0 failed; 0 ignored    crypto
test result: ok.  88 passed; 0 failed; 0 ignored    domain
test result: ok.  24 passed; 0 failed; 0 ignored    engine
test result: ok.   9 passed; 0 failed; 0 ignored    queue
test result: ok.  18 passed; 0 failed; 0 ignored    simulator
test result: ok.  66 passed; 0 failed; 0 ignored    store
test result: ok.   3 passed; 0 failed; 0 ignored    webhooks
```

### What the black-box suite actually asserts

The names are the documentation. A selection:

```text
full_success_flow_over_http
authorize_card_then_capture_endpoint
unknown_card_is_rejected_with_no_payment_created
no_response_ever_echoes_a_full_card_number
a_malformed_cvc_is_a_card_error_that_never_quotes_the_value
publishable_key_is_rejected_with_guidance
every_response_carries_a_request_id_and_errors_repeat_it_in_the_body
a_caller_supplied_request_id_is_adopted
an_abusive_request_id_is_replaced_rather_than_echoed
a_repeated_key_replays_the_original_response_instead_of_charging_twice
idempotency_keys_are_scoped_per_merchant
a_malformed_idempotency_key_is_rejected_before_any_work
a_declined_payment_replays_as_the_same_decline
a_collection_reports_has_more_and_hands_back_a_cursor
an_out_of_range_limit_is_an_error_rather_than_a_silent_clamp
events_are_paginated_like_every_other_collection
webhook_endpoint_secret_shown_once
a_dangerous_webhook_url_is_refused
deleting_an_endpoint_stops_it_from_being_listed_and_is_idempotent
an_unknown_field_is_rejected_and_named
a_wrongly_typed_field_names_its_path
health_is_open_and_uncacheable
```

These run against the **real router** — the whole middleware stack, the real error envelope, the
real database — via `tower::ServiceExt::oneshot`. No layer in `router()` is allowed to change the
return type, precisely so tests can drive it directly.

---

## Smoke test

```bash
docker compose up -d
export DATABASE_URL=postgres://sandbox:sandbox@localhost:54432/sandbox_dev
SERVER_PORT=8099 cargo run -p sandbox-server

# probes
curl -i localhost:8099/health                                     # 200 + Cache-Control: no-store
curl -i localhost:8099/v1/orders                                  # 401 in the envelope
curl -i -X PUT localhost:8099/health                              # 405 in the envelope
curl -i localhost:8099/v1/nope -H 'Authorization: Bearer sk_...'  # 404 unknown_endpoint

# the loop
curl -s localhost:8099/v1/orders \
  -H 'Authorization: Bearer sk_...' -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: order-demo-0001' \
  -d '{"amount":50000,"currency":"INR","receipt":"rcpt_1"}'

# repeat the exact same call → identical body, plus `idempotent-replayed: true`
```

---

## Dashboard (in progress)

The API is the product; the dashboard is how you *see* it work. It is the next milestone, and this
section will fill in with screenshots as each view lands.

Planned views:

| View | Shows |
|---|---|
| **Payments timeline** | Every attempt with its simulated latency, decision, and resulting status — the request/response pair side by side |
| **Ledger explorer** | Each transaction's entries with running balances, and the balance-to-zero proof rendered live |
| **Webhook delivery log** | Attempt history, response codes, signature headers, and the position on the retry ladder, with a manual replay button |
| **Event stream** | The `/v1/events` feed as a live tail |
| **Test card rack** | One-click payment with any card in the table, so the decision matrix is explorable rather than documented |
| **API key management** | Mint, scope, and revoke keys — exercising the `AuthCache::invalidate_key` path |

<!--
  SCREENSHOTS — drop files into docs/screenshots/ and uncomment.

  ### Payments timeline
  ![Payments timeline](docs/screenshots/payments-timeline.png)

  ### Ledger explorer
  ![Ledger explorer](docs/screenshots/ledger-explorer.png)

  ### Webhook delivery log
  ![Webhook delivery log](docs/screenshots/webhook-deliveries.png)

  ### Test card rack
  ![Test card rack](docs/screenshots/test-cards.png)
-->

> **Screenshots go here.** Add images to `docs/screenshots/` and uncomment the block above as each
> view is built.

---

## Roadmap

The schema and the store layer for the next three milestones already exist — `disputes`,
`settlements`, and `audit_logs` are migrated, with row structs and queries written. What is missing
is the orchestration and the HTTP surface.

### v2 — disputes

- The `4000000000000259` card already succeeds and is *meant* to open a dispute 60 seconds later.
  The job kind is defined; the handler is not registered yet, so those jobs currently die cleanly
  as "no handler registered".
- `DisputeReason` and `DisputeStatus` exist, with evidence deadlines — the pressure that makes
  disputes worth teaching.
- Funds move to `dispute_holding` on open, back on win, out on loss.
- Endpoints: `GET /v1/disputes`, `GET /v1/disputes/{id}`, `POST /v1/disputes/{id}/evidence`.

### v2 — settlements

- The T+2 sweep: cleared funds move `merchant_pending → merchant_available` and a payout record is
  written.
- Reconciliation CSV generated from the entries in the period.
- This is also when refunds gain a real `pending → processed` lifecycle. Today they settle
  instantly, because there is no bank to wait for and an integrator learns nothing from a delay
  whose end they cannot observe. The `pending` state is already in the schema.

### v2 — risk review

- `RiskHold` currently parks the payment at `created`. The review handler will resolve it to
  captured or failed after a delay.
- Velocity checks keyed on the card fingerprint — which is exactly what the peppered fingerprint
  was built for.

### v3 — checkout

- A hosted checkout page authenticated by the **publishable** key, which is why `pk_test_` keys
  exist with zero server scopes today.
- The handoff signature: when checkout closes, the browser hands
  `{order_id, payment_id, signature}` back to the merchant's page, and the merchant **must** verify
  it server-side before marking their order paid. Without it, anyone can call their success handler
  from the browser console and claim they paid. This is the single most commonly skipped step in
  real integrations, and `crypto::signing` already implements both halves.

### Ongoing

- 3-D Secure challenge flow for `requires_action` payments.
- A card vault using the AES-GCM envelope encryption that `CRYPTO_MASTER_KEY` is already validated
  for.
- Per-currency minimum amounts, so the fee can never exceed the payment.

---

## Known limitations

These are tracked, not undiscovered. Each one is written down in the source at the place it
matters.

### A timed-out request can wedge its idempotency key

`timeout::enforce` sits **outside** `idempotency::enforce`. On timeout, `tokio::time::timeout`
drops the inner future, so idempotency's `record()`/`release()` never run after `acquire()` has
already written the row as `in_progress`. `store::idempotency` only clears `in_progress` rows in
`purge_expired` at `expires_at` (24 h), so retries get `409 idempotency_key_in_flight` for a day —
which contradicts the 504 message telling the caller to *"retry with the same Idempotency-Key to
find out whether it completed."*

**Do not fix this by moving the timeout inside idempotency.** That would release the key on 504 and
reopen the double-charge window. The intended fix is to bound the stale window: let
`store::idempotency::acquire` take over an `in_progress` row older than the request budget plus a
margin, driven by a new `stale_in_flight_after` field on `ApiConfig`. Not yet implemented — it
touches the `store` crate.

### The pagination cursor is `created_at` only

Rows sharing a `created_at` to the microsecond can be skipped at a page boundary. The fix is a
composite `(created_at, id)` cursor in the store's `WHERE` clause plus a matching index.
Deliberately deferred; documented in [crates/api/pagination.rs](crates/api/pagination.rs).

### Housekeeping

`crates/store/src/Untitled-1.rs` is a stray file that should be removed.

---

## FAQ

**Why not just use Stripe's test mode?**
You cannot read Stripe's ledger, watch its outbox, or break its lock ordering to see what happens.
This is a gateway you can open up. The whole point is that the interesting parts — the balancing
trigger, the transactional outbox, the lock ranks — are visible and modifiable.

**Why Rust?**
A payments system is exactly the domain where "this state is unrepresentable" beats "we have a test
for that". `OrderId` and `PaymentId` being distinct types, `BalancedTransaction` having no
unbalanced constructor, and `AccountType::normal_balance()` failing to compile when you add a
variant are all correctness that costs nothing at runtime.

**Why integer minor units instead of a decimal type?**
Because `clippy::float_arithmetic` being denied workspace-wide is a stronger guarantee than a
convention, and because every real gateway API speaks minor units on the wire. A decimal type would
be defensible; a float never is.

**Can I point real card numbers at this?**
No, and it will refuse. `simulator::decide` returns `NotATestCard` for anything outside the
published table, and it runs *before* any database write. That is the design's single most
important safety property.

**Why is a cross-merchant request a 404 rather than a 403?**
A `403` confirms the resource exists. Given a `403` on `order_abc`, an attacker has learned
something true. Making ownership failures indistinguishable from "does not exist" means ids cannot
be probed.

**Why does an unknown `/v1` path return 401 without a key?**
Auth sits outside the router fallback, deliberately. An unauthenticated caller cannot map which
paths exist.

**What does `SIMULATE_LATENCY` actually do?**
Sleeps for the simulator's decided latency (300–800 ms, derived deterministically from the card
digits) before responding to payment creation, capped at `max_simulated_latency`. It exists so
integrators do not build UIs that assume payments resolve instantly. Tests turn it off.

---

## License

Not yet licensed — there is no `LICENSE` file in the repository. Until one is added, default
copyright applies and the code is not open source. Adding an MIT `LICENSE` is the intended next
step.

---

<div align="center">

**TEST MODE ONLY — no real money moves through this system, by construction.**

</div>
