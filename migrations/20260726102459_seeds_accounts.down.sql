-- Undo exactly the rows this seed inserted (platform-scoped accounts of the
-- three seeded types, across the five seeded currencies) rather than
-- truncating the table, which could also hold merchant-scoped accounts
-- created on demand since this migration ran.
DELETE FROM ledger_accounts
WHERE merchant_id IS NULL
  AND account_type IN ('gateway_clearing', 'platform_revenue', 'tax_payable')
  AND currency IN ('INR', 'USD', 'EUR', 'JPY', 'KWD');
