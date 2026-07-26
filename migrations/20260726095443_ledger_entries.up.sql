-- Individual postings. Sign convention: positive = credit, negative = debit.
-- These rows are APPEND-ONLY and IMMUTABLE — migration 0013 installs triggers
-- that make UPDATE and DELETE raise an exception. The books are a permanent
-- record; you correct a mistake by posting a compensating transaction, never
-- by editing history.
CREATE TABLE ledger_entries (
    id              UUID PRIMARY KEY,
    transaction_id  UUID NOT NULL REFERENCES ledger_transactions(id),
    account_id      UUID NOT NULL REFERENCES ledger_accounts(id),

    amount_minor    BIGINT NOT NULL,
    currency        CHAR(3) NOT NULL,

    created_at      TIMESTAMPTZ NOT NULL,

    -- A zero-amount entry carries no information and is always a caller bug,
    -- matching LedgerEntry's rejection of zero in the domain crate.
    CONSTRAINT ledger_entry_nonzero  CHECK (amount_minor <> 0),
    CONSTRAINT ledger_entry_currency CHECK (currency ~ '^[A-Z]{3}$')
);

CREATE INDEX ledger_entry_txn_idx     ON ledger_entries (transaction_id);
CREATE INDEX ledger_entry_account_idx ON ledger_entries (account_id);

-- The hot path: "what is this account's balance?" is
--   SELECT SUM(amount_minor) FROM ledger_entries WHERE account_id = $1
-- so the account_id index above is what keeps balance lookups fast.