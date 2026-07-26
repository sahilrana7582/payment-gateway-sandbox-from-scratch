CREATE TYPE settlement_status AS ENUM ('pending', 'processed');

-- A settlement batch: the T+2 sweep that moves a merchant's cleared funds from
-- pending to available and records the payout. Each settlement summarizes a
-- period and its net amount (gross minus fees, tax, and refund offsets). The
-- downloadable reconciliation CSV is generated from the entries in this period.
CREATE TABLE settlements (
    id              UUID PRIMARY KEY,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    currency        CHAR(3) NOT NULL,

    period_start    TIMESTAMPTZ NOT NULL,
    period_end      TIMESTAMPTZ NOT NULL,

    gross_minor     BIGINT NOT NULL,   -- total captured in period
    fees_minor      BIGINT NOT NULL,   -- platform fees deducted
    tax_minor       BIGINT NOT NULL,   -- tax on fees
    refunds_minor   BIGINT NOT NULL,   -- refunds offset in period
    net_minor       BIGINT NOT NULL,   -- what actually pays out

    status          settlement_status NOT NULL DEFAULT 'pending',

    created_at      TIMESTAMPTZ NOT NULL,
    processed_at    TIMESTAMPTZ,

    CONSTRAINT settlement_currency_shape CHECK (currency ~ '^[A-Z]{3}$'),
    CONSTRAINT settlement_period_valid   CHECK (period_end > period_start)
);

CREATE INDEX settlements_merchant_idx ON settlements (merchant_id);
CREATE INDEX settlements_status_idx   ON settlements (status);