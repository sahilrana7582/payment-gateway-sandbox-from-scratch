CREATE TYPE order_status AS ENUM ('created', 'attempted', 'paid', 'expired');

CREATE TABLE orders (
    id              UUID PRIMARY KEY,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

    -- Money is always (amount_minor, currency). Never a single float column.
    amount_minor    BIGINT NOT NULL,
    currency        CHAR(3) NOT NULL,

    -- Cumulative captured amount. Starts at 0, set when a payment captures.
    amount_paid     BIGINT NOT NULL DEFAULT 0,

    -- Merchant's own reference (Razorpay calls it `receipt`). How their system
    -- re-finds this order. Not unique globally, but unique per merchant when set.
    receipt         TEXT,

    -- Free-form merchant metadata, echoed verbatim in webhooks.
    notes           JSONB NOT NULL DEFAULT '{}',

    status          order_status NOT NULL DEFAULT 'created',
    created_at      TIMESTAMPTZ NOT NULL,

    CONSTRAINT orders_amount_positive     CHECK (amount_minor >= 100),
    CONSTRAINT orders_amount_paid_bounds  CHECK (amount_paid >= 0 AND amount_paid <= amount_minor),
    CONSTRAINT orders_currency_shape      CHECK (currency ~ '^[A-Z]{3}$')
);

CREATE INDEX orders_merchant_id_idx      ON orders (merchant_id);
CREATE INDEX orders_status_idx           ON orders (status);
CREATE INDEX orders_created_at_idx       ON orders (created_at DESC);

-- A merchant's receipts must be unique when provided, so their idempotent
-- "create order for cart X" logic can rely on it. NULLs are allowed and not
-- constrained (an order without a receipt is valid).
CREATE UNIQUE INDEX orders_merchant_receipt_idx
    ON orders (merchant_id, receipt)
    WHERE receipt IS NOT NULL;
