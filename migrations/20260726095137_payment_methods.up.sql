CREATE TYPE payment_method_type AS ENUM ('card');

CREATE TYPE card_brand AS ENUM (
    'visa', 'mastercard', 'amex', 'rupay', 'discover', 'unknown'
);

-- Stored, reusable card metadata. There is NO column for a full PAN or a CVC,
-- by design — the schema itself cannot hold cardholder data. `fingerprint`
-- (HMAC of the PAN) enables duplicate-card detection without retention.
CREATE TABLE payment_methods (
    id              UUID PRIMARY KEY,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
    type            payment_method_type NOT NULL DEFAULT 'card',

    last4           CHAR(4) NOT NULL,
    brand           card_brand NOT NULL,
    exp_month       SMALLINT NOT NULL,
    exp_year        SMALLINT NOT NULL,
    fingerprint     TEXT NOT NULL,

    created_at      TIMESTAMPTZ NOT NULL,

    CONSTRAINT pm_last4_shape   CHECK (last4 ~ '^[0-9]{4}$'),
    CONSTRAINT pm_month_range   CHECK (exp_month BETWEEN 1 AND 12),
    CONSTRAINT pm_year_range    CHECK (exp_year BETWEEN 2000 AND 2100)
);

CREATE INDEX pm_merchant_id_idx   ON payment_methods (merchant_id);
CREATE INDEX pm_fingerprint_idx   ON payment_methods (fingerprint);