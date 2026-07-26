CREATE TYPE ledger_transaction_kind AS ENUM (
    'payment_captured',
    'fee_charged',
    'refund_issued',
    'fee_refunded',
    'dispute_opened',
    'dispute_won',
    'dispute_lost',
    'funds_released',
    'payout_paid',
    'reserve_held',
    'reserve_released'
);

-- The header row for a set of balanced entries. `reference_type` /
-- `reference_id` link the transaction back to what caused it (a payment, a
-- refund, a dispute), so the dashboard can explain every movement of money.
CREATE TABLE ledger_transactions (
    id              UUID PRIMARY KEY,
    kind            ledger_transaction_kind NOT NULL,

    reference_type  TEXT,       -- 'payment' | 'refund' | 'dispute' | 'settlement'
    reference_id    UUID,

    description     TEXT,
    created_at      TIMESTAMPTZ NOT NULL
);

CREATE INDEX ledger_txn_reference_idx ON ledger_transactions (reference_type, reference_id);
CREATE INDEX ledger_txn_kind_idx      ON ledger_transactions (kind);
CREATE INDEX ledger_txn_created_idx   ON ledger_transactions (created_at DESC);