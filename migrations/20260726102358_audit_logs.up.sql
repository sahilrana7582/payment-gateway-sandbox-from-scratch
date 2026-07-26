-- An append-only record of every sensitive action: key created/revoked,
-- refund issued, webhook endpoint changed, settings modified. Independent of
-- the ledger and events — this is the security/compliance trail, answering
-- "who did what, when, from where."
CREATE TABLE audit_logs (
    id              UUID PRIMARY KEY,
    merchant_id     UUID REFERENCES merchants(id) ON DELETE SET NULL,

    actor_type      TEXT NOT NULL,   -- 'api_key' | 'dashboard_user' | 'system'
    actor_id        TEXT,

    action          TEXT NOT NULL,   -- 'api_key.created', 'refund.issued', ...
    resource_type   TEXT,
    resource_id     TEXT,

    before_state    JSONB,
    after_state     JSONB,

    ip_address      TEXT,

    created_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX audit_logs_merchant_idx  ON audit_logs (merchant_id);
CREATE INDEX audit_logs_action_idx    ON audit_logs (action);
CREATE INDEX audit_logs_created_idx   ON audit_logs (created_at DESC);
CREATE INDEX audit_logs_resource_idx  ON audit_logs (resource_type, resource_id);