-- Add up migration script here
CREATE TYPE api_key_kind AS ENUM ('secret', 'publishable');

CREATE TABLE api_keys (
    id              UUID PRIMARY KEY,
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
    kind            api_key_kind NOT NULL,
    label           TEXT NOT NULL,
    scopes          TEXT[] NOT NULL DEFAULT '{}',
    -- Argon2id hash of the secret. The plaintext is NEVER stored — it is
    -- shown to the merchant once, at creation, then discarded.
    secret_hash     TEXT NOT NULL,
    last_used_at    TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX api_keys_merchant_id_idx ON api_keys (merchant_id);

-- Lookup path for auth middleware: given a bearer secret, we need the row
-- fast. We index on the hash rather than any plaintext fragment.
CREATE UNIQUE INDEX api_keys_secret_hash_idx ON api_keys (secret_hash);