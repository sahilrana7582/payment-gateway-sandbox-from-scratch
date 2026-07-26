-- Add up migration script here
CREATE TYPE webhook_delivery_status AS ENUM (
    'pending', 'delivered', 'failed', 'dead'
);

-- One row per (event, endpoint, attempt). This is the delivery log the
-- dashboard renders, with response codes and a manual resend button. The
-- retry ladder (1m, 5m, 30m, 2h, 6h, 24h, then 'dead') is driven by
-- next_retry_at, which the job worker reads.
CREATE TABLE webhook_deliveries (
    id              UUID PRIMARY KEY,
    event_id        UUID NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    endpoint_id     UUID NOT NULL REFERENCES webhook_endpoints(id) ON DELETE CASCADE,

    attempt         INT NOT NULL DEFAULT 1,
    status          webhook_delivery_status NOT NULL DEFAULT 'pending',

    -- Captured from the merchant's endpoint. Body truncated to 2KB — we log
    -- enough to debug without letting a merchant's huge response fill our disk.
    response_status INT,
    response_body   TEXT,
    duration_ms     INT,

    -- When the next attempt should run. NULL once delivered or dead.
    next_retry_at   TIMESTAMPTZ,

    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,

    CONSTRAINT webhook_delivery_attempt_positive CHECK (attempt >= 1)
);

CREATE INDEX webhook_deliveries_event_idx    ON webhook_deliveries (event_id);
CREATE INDEX webhook_deliveries_endpoint_idx ON webhook_deliveries (endpoint_id);
CREATE INDEX webhook_deliveries_status_idx   ON webhook_deliveries (status);

-- The worker polls "what's due for retry?" — this partial index keeps that
-- query cheap by only indexing rows that are actually awaiting a retry.
CREATE INDEX webhook_deliveries_due_idx
    ON webhook_deliveries (next_retry_at)
    WHERE status = 'pending' AND next_retry_at IS NOT NULL;