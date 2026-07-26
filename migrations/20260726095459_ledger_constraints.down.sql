DROP TRIGGER ledger_entries_balance_check ON ledger_entries;
DROP FUNCTION ledger_transaction_must_balance();

DROP TRIGGER ledger_entries_no_delete ON ledger_entries;
DROP TRIGGER ledger_entries_no_update ON ledger_entries;
DROP FUNCTION ledger_entries_forbid_mutation();
