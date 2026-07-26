CREATE TYPE dispute_reason AS ENUM (
    'fraudulent',
    'duplicate',
    'product_not_received',
    'product_unacceptable',
    'subscription_cancelled',
    'unrecognized',
    'credit_not_processed',
    'general'
);

CREATE TYPE dispute_status AS ENUM ('open', 'under_review', 'won', 'lost');

-- A chargeback raised by the customer's bank against a captured payment. The
-- merchant must submit evidence by `respond_by` or the dispute auto-resolves
-- against them, exactly like Razorpay's real dispute lifecycle.
CREATE TABLE disputes (
    id              UUID PRIMARY KEY,
    payment_id      UUID NOT NULL REFERENCES payments(id) ON DELETE CASCADE,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    amount_minor    BIGINT NOT NULL,
    currency        CHAR(3) NOT NULL,

    reason          dispute_reason NOT NULL,
    status          dispute_status NOT NULL DEFAULT 'open',

    -- Deadline for the merchant to submit evidence. Past this point the
    -- simulator resolves the dispute automatically (see the disputes worker).
    respond_by      TIMESTAMPTZ NOT NULL,

    -- Merchant-submitted evidence (free text, receipt URLs, etc.), for the
    -- dashboard's dispute detail view. Never contains cardholder data.
    evidence        JSONB NOT NULL DEFAULT '{}',
    notes           JSONB NOT NULL DEFAULT '{}',

    created_at      TIMESTAMPTZ NOT NULL,
    resolved_at     TIMESTAMPTZ,

    CONSTRAINT dispute_amount_positive CHECK (amount_minor > 0),
    CONSTRAINT dispute_currency_shape  CHECK (currency ~ '^[A-Z]{3}$'),
    -- A resolved dispute (won/lost) must carry a resolution timestamp; an
    -- open/under_review one must not — same pattern as pay_captured_at_consistency.
    CONSTRAINT dispute_resolved_at_consistency CHECK (
        (status IN ('won', 'lost') AND resolved_at IS NOT NULL)
        OR (status IN ('open', 'under_review') AND resolved_at IS NULL)
    )
);

CREATE INDEX dispute_payment_id_idx  ON disputes (payment_id);
CREATE INDEX dispute_merchant_id_idx ON disputes (merchant_id);
CREATE INDEX dispute_status_idx      ON disputes (status);

-- The disputes worker polls for "still open and past its deadline" — this
-- partial index keeps that scan cheap as the table grows.
CREATE INDEX dispute_respond_by_idx
    ON disputes (respond_by)
    WHERE status IN ('open', 'under_review');
