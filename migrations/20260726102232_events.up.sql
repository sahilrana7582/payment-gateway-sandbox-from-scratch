-- Every state change that a merchant might care about produces an event row.
-- Events are the source of truth for webhooks: the webhook worker reads from
-- here. Events are written in the SAME transaction as the state change that
-- caused them (transactional outbox pattern) so an event can never exist for a
-- payment that rolled back, and a committed payment can never lack its event.
CREATE TABLE events (
    id              UUID PRIMARY KEY,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    -- e.g. 'payment.captured', 'payment.failed', 'refund.processed',
    -- 'order.paid', 'dispute.opened'.
    type            TEXT NOT NULL,

    -- The full JSON payload delivered to webhooks. Snapshotted at emit time so
    -- later mutations to the underlying row don't change what was sent.
    payload         JSONB NOT NULL,

    -- Versioning the payload shape lets us evolve it without breaking old
    -- integrations, exactly as Razorpay/Stripe version their APIs.
    api_version     TEXT NOT NULL,

    created_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX events_merchant_id_idx ON events (merchant_id);
CREATE INDEX events_type_idx        ON events (type);
CREATE INDEX events_created_at_idx  ON events (created_at DESC);