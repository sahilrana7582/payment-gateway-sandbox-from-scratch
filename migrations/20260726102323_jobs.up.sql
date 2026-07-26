CREATE TYPE job_state AS ENUM ('pending', 'running', 'completed', 'failed', 'dead');

-- The background job queue. Postgres IS the queue — no Redis, no broker. Jobs
-- are claimed with SELECT ... FOR UPDATE SKIP LOCKED, which lets multiple
-- workers pull disjoint batches concurrently without blocking each other.
--
-- The critical property: a job is enqueued in the SAME transaction as the
-- state change that needs it (e.g. a webhook job enqueued alongside the
-- payment capture). If that transaction rolls back, the job vanishes with it —
-- the transactional outbox pattern, correct by construction.
CREATE TABLE jobs (
    id              UUID PRIMARY KEY,

    -- e.g. 'deliver_webhook', 'expire_order', 'release_settlement',
    -- 'open_dispute', 'purge_old_data'.
    kind            TEXT NOT NULL,
    payload         JSONB NOT NULL DEFAULT '{}',

    state           job_state NOT NULL DEFAULT 'pending',

    -- When this job becomes eligible to run. Future-dated for scheduled work
    -- (a webhook retry, a T+2 settlement) — the worker only claims rows whose
    -- run_at <= now().
    run_at          TIMESTAMPTZ NOT NULL,

    attempts        INT NOT NULL DEFAULT 0,
    max_attempts    INT NOT NULL DEFAULT 6,
    last_error      TEXT,

    -- Lease fields. When a worker claims a job it stamps locked_at/locked_by;
    -- the reaper releases jobs whose lease has expired (crashed worker).
    locked_at       TIMESTAMPTZ,
    locked_by       TEXT,

    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,

    CONSTRAINT jobs_attempts_bounds CHECK (attempts >= 0 AND attempts <= max_attempts)
);

-- The claim query orders by run_at among pending, due jobs. This partial index
-- is exactly shaped to that query so claiming stays O(batch), not O(table).
CREATE INDEX jobs_claimable_idx
    ON jobs (run_at)
    WHERE state = 'pending';

-- The reaper scans for stale leases.
CREATE INDEX jobs_lease_idx
    ON jobs (locked_at)
    WHERE state = 'running';

CREATE INDEX jobs_kind_idx ON jobs (kind);