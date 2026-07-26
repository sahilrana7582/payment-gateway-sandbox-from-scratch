-- Platform-scoped ledger accounts are singletons per currency — there is
-- exactly one gateway_clearing INR account for the whole system, etc. Seed
-- them here so the engine never has to create-or-find them on the hot path;
-- they simply always exist.
--
-- Merchant-scoped accounts (merchant_pending, etc.) are created on demand when
-- a merchant first transacts in a currency — those can't be seeded because
-- merchants don't exist yet at migration time.
--
-- gen_random_uuid() (from pgcrypto, migration 0001) is fine here: these rows
-- are created once at deploy time, not on any latency-sensitive path, so the
-- UUIDv7 ordering benefit we rely on elsewhere doesn't matter.

INSERT INTO ledger_accounts (id, merchant_id, account_type, currency, created_at)
SELECT
    gen_random_uuid(),
    NULL,
    account_type::ledger_account_type,
    currency,
    now()
FROM
    (VALUES ('gateway_clearing'), ('platform_revenue'), ('tax_payable')) AS t(account_type)
CROSS JOIN
    (VALUES ('INR'), ('USD'), ('EUR'), ('JPY'), ('KWD')) AS c(currency);

-- That's 3 account types x 5 currencies = 15 platform accounts seeded.