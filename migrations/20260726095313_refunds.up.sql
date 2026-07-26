-- Add up migration script here
CREATE TYPE refund_status AS ENUM ('pending', 'processed', 'failed');

CREATE TABLE refunds (
    id              UUID PRIMARY KEY,
    payment_id      UUID NOT NULL REFERENCES payments(id) ON DELETE CASCADE,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    amount_minor    BIGINT NOT NULL,
    currency        CHAR(3) NOT NULL,

    reason          TEXT,
    status          refund_status NOT NULL DEFAULT 'pending',
    notes           JSONB NOT NULL DEFAULT '{}',

    created_at      TIMESTAMPTZ NOT NULL,
    processed_at    TIMESTAMPTZ,

    CONSTRAINT refund_amount_positive CHECK (amount_minor > 0),
    CONSTRAINT refund_currency_shape  CHECK (currency ~ '^[A-Z]{3}$')
);

CREATE INDEX refund_payment_id_idx  ON refunds (payment_id);
CREATE INDEX refund_merchant_id_idx ON refunds (merchant_id);
CREATE INDEX refund_status_idx      ON refunds (status);

-- The over-refund guard lives in the engine layer under a row lock (summing
-- existing refunds against the payment amount inside the same transaction).
-- We do NOT put a cross-row CHECK here because CHECK constraints cannot query
-- other rows — enforcing it in application code under SELECT ... FOR UPDATE is
-- the correct pattern, and teaching that is part of the point.