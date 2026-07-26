CREATE TYPE idempotency_state AS ENUM ('in_progress', 'completed');

-- Makes mutating requests safe to retry. A client sends an Idempotency-Key
-- header; if the same key is replayed, we return the ORIGINAL response instead
-- of performing the action twice. This is what makes payments safe under
-- network retries — the single most important reliability feature, and the one
-- most tutorials skip.
--
-- Algorithm (implemented in the api crate's idempotency middleware):
--   1. INSERT ... ON CONFLICT DO NOTHING with state 'in_progress'.
--   2. If we inserted: we own the request. Execute, store response, set 'completed'.
--   3. If conflict + existing 'completed': compare request_hash.
--        match    -> replay stored response verbatim.
--        mismatch -> 409 (same key, different body = client bug).
--   4. If conflict + existing 'in_progress': 409 request_in_progress (don't wait).
CREATE TABLE idempotency_keys (
    -- The composite (merchant_id, key) is the real identity; a surrogate id
    -- keeps foreign-key-free simplicity.
    merchant_id     UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
    key             TEXT NOT NULL,

    -- Which endpoint the key was used against, so the same key on a different
    -- route is treated as distinct.
    endpoint        TEXT NOT NULL,

    -- Hash of the request body. Detects "same key, different payload" misuse.
    request_hash    TEXT NOT NULL,

    state           idempotency_state NOT NULL DEFAULT 'in_progress',

    -- The stored original response, replayed on a matching retry.
    response_status INT,
    response_body   JSONB,

    -- Keys expire after 24h; a purge job clears them.
    expires_at      TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL,

    PRIMARY KEY (merchant_id, key)
);

CREATE INDEX idempotency_expires_idx ON idempotency_keys (expires_at);