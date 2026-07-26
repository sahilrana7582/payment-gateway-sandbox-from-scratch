-- Add up migration script here
-- UUIDv7 generation is done in Rust (uuid crate), not Postgres — this
-- extension is kept only for pgcrypto's gen_random_bytes(), used later by
-- the idempotency and audit_log migrations.
CREATE EXTENSION IF NOT EXISTS pgcrypto;