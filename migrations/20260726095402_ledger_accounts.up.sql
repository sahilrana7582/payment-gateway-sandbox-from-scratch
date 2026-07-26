CREATE TYPE ledger_account_type AS ENUM (
    'gateway_clearing',
    'merchant_pending',
    'merchant_available',
    'merchant_reserve',
    'dispute_holding',
    'platform_revenue',
    'tax_payable'
);

-- One account per (merchant, type, currency) for merchant-scoped types, and
-- one per (type, currency) for platform-scoped types. There is deliberately NO
-- balance column — balance is always SUM(ledger_entries.amount) for the
-- account. A stored balance can drift from the entries; a derived one cannot.
CREATE TABLE ledger_accounts (
    id              UUID PRIMARY KEY,

    -- NULL for platform accounts (gateway_clearing, platform_revenue, tax_payable).
    -- NOT NULL for merchant accounts.
    merchant_id     UUID REFERENCES merchants(id) ON DELETE CASCADE,

    account_type    ledger_account_type NOT NULL,
    currency        CHAR(3) NOT NULL,

    created_at      TIMESTAMPTZ NOT NULL,

    CONSTRAINT ledger_acct_currency_shape CHECK (currency ~ '^[A-Z]{3}$'),

    -- Enforce the scoping invariant at the database level, mirroring
    -- LedgerAccount::from_parts in the domain crate. Platform types must have
    -- NULL merchant_id; merchant types must have a non-NULL merchant_id.
    CONSTRAINT ledger_acct_scoping CHECK (
        (account_type IN ('gateway_clearing', 'platform_revenue', 'tax_payable')
            AND merchant_id IS NULL)
        OR
        (account_type IN ('merchant_pending', 'merchant_available', 'merchant_reserve', 'dispute_holding')
            AND merchant_id IS NOT NULL)
    )
);

-- One merchant account per (merchant, type, currency).
CREATE UNIQUE INDEX ledger_acct_merchant_uniq
    ON ledger_accounts (merchant_id, account_type, currency)
    WHERE merchant_id IS NOT NULL;

-- One platform account per (type, currency).
CREATE UNIQUE INDEX ledger_acct_platform_uniq
    ON ledger_accounts (account_type, currency)
    WHERE merchant_id IS NULL;