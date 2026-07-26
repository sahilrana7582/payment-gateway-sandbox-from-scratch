-- Add up migration script here
-- One row per confirmation attempt on a payment. Makes retry behavior and the
-- exact simulator decision visible for debugging and for the dashboard's
-- attempt timeline. A payment normally has one attempt; a card that requires
-- action then completes has two.
CREATE TABLE payment_attempts (
    id                  UUID PRIMARY KEY,
    payment_id          UUID NOT NULL REFERENCES payments(id) ON DELETE CASCADE,

    attempt_number      INT NOT NULL,

    -- The simulator's verdict for this attempt.
    outcome             TEXT NOT NULL,          -- 'success' | 'declined' | 'requires_action' | 'risk_hold'
    decline_code        TEXT,                    -- populated when outcome = 'declined'

    -- Full simulated network response, for the dashboard's raw view.
    network_response    JSONB NOT NULL DEFAULT '{}',

    -- Simulated latency this attempt "took", so integrators see realistic timing.
    latency_ms          INT NOT NULL DEFAULT 0,

    created_at          TIMESTAMPTZ NOT NULL,

    CONSTRAINT attempt_number_positive CHECK (attempt_number >= 1)
);

CREATE INDEX attempt_payment_id_idx ON payment_attempts (payment_id, attempt_number);