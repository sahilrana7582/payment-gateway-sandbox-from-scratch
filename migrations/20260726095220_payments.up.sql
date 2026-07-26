CREATE TYPE payment_status AS ENUM (
    'created', 'requires_action', 'authorized', 'captured', 'failed', 'refunded'
);

CREATE TABLE payments (
    id                  UUID PRIMARY KEY,
    order_id            UUID NOT NULL REFERENCES orders(id) ON DELETE CASCADE,

    -- Denormalized from the order so authorization checks avoid a join.
    merchant_id         UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    amount_minor        BIGINT NOT NULL,
    currency            CHAR(3) NOT NULL,

    -- Cumulative refunded amount. Starts 0, rises with each refund.
    amount_refunded     BIGINT NOT NULL DEFAULT 0,

    method              payment_method_type NOT NULL DEFAULT 'card',

    -- Inline card metadata for this attempt. Nullable because non-card methods
    -- (v2) won't populate it. Never contains a PAN or CVC.
    card_last4          CHAR(4),
    card_brand          card_brand,
    card_exp_month      SMALLINT,
    card_exp_year       SMALLINT,
    card_fingerprint    TEXT,

    status              payment_status NOT NULL DEFAULT 'created',

    -- Populated only on failure.
    error_code          TEXT,
    error_description   TEXT,

    notes               JSONB NOT NULL DEFAULT '{}',

    created_at          TIMESTAMPTZ NOT NULL,
    captured_at         TIMESTAMPTZ,

    CONSTRAINT pay_amount_positive      CHECK (amount_minor >= 100),
    CONSTRAINT pay_refund_bounds        CHECK (amount_refunded >= 0 AND amount_refunded <= amount_minor),
    CONSTRAINT pay_currency_shape       CHECK (currency ~ '^[A-Z]{3}$'),
    -- A captured payment must have a capture timestamp; a non-captured one must not.
    CONSTRAINT pay_captured_at_consistency CHECK (
        (status IN ('captured', 'refunded') AND captured_at IS NOT NULL)
        OR (status NOT IN ('captured', 'refunded') AND captured_at IS NULL)
    ),
    -- A failed payment must carry an error code.
    CONSTRAINT pay_failure_has_code CHECK (
        (status = 'failed' AND error_code IS NOT NULL)
        OR (status <> 'failed')
    )
);

CREATE INDEX pay_order_id_idx       ON payments (order_id);
CREATE INDEX pay_merchant_id_idx    ON payments (merchant_id);
CREATE INDEX pay_status_idx         ON payments (status);
CREATE INDEX pay_created_at_idx     ON payments (created_at DESC);
CREATE INDEX pay_fingerprint_idx    ON payments (card_fingerprint) WHERE card_fingerprint IS NOT NULL;