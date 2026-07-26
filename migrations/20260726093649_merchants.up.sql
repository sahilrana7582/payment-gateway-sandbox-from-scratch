-- Add up migration script here
CREATE TABLE merchants (
    id                          UUID PRIMARY KEY,
    name                        TEXT NOT NULL,
    email                       TEXT NOT NULL,
    checkout_allowed_domains    TEXT[] NOT NULL DEFAULT '{}',
    created_at                  TIMESTAMPTZ NOT NULL,

    CONSTRAINT merchants_name_not_empty CHECK (btrim(name) <> ''),
    CONSTRAINT merchants_email_shape CHECK (email LIKE '%@%.%')
);

CREATE UNIQUE INDEX merchants_email_idx ON merchants (lower(email));