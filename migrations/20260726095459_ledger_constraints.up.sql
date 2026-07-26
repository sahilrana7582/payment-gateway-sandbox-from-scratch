-- ============================================================================
-- LEDGER INTEGRITY ENFORCEMENT
--
-- This migration is the database-level guarantee that backs the Rust
-- BalancedTransaction type. Two independent layers now protect the books:
--   1. The domain crate: BalancedTransaction cannot be constructed unbalanced.
--   2. This migration: the database rejects an unbalanced or mutated ledger
--      even if a bug, a raw SQL script, or a future code path bypasses layer 1.
--
-- Defense in depth. A bug in either layer is caught by the other.
-- ============================================================================


-- ----------------------------------------------------------------------------
-- 1. IMMUTABILITY: ledger entries can never be updated or deleted.
-- ----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION ledger_entries_forbid_mutation()
RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'ledger_entries are immutable: % is not permitted', TG_OP
        USING ERRCODE = 'integrity_constraint_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ledger_entries_no_update
    BEFORE UPDATE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_entries_forbid_mutation();

CREATE TRIGGER ledger_entries_no_delete
    BEFORE DELETE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION ledger_entries_forbid_mutation();


-- ----------------------------------------------------------------------------
-- 2. ZERO-SUM: every transaction's entries must sum to exactly zero, per
--    currency. This is THE double-entry invariant.
--
-- We use a CONSTRAINT TRIGGER that is DEFERRABLE INITIALLY DEFERRED. This is
-- essential: entries are inserted one row at a time, so the sum is only
-- correct once ALL of a transaction's entries are in. Deferring the check to
-- COMMIT time lets us insert entry 1 (sum != 0, temporarily fine) then entry 2
-- (sum == 0) and validate only at the end.
--
-- If a transaction commits with entries that don't net to zero in every
-- currency, the COMMIT itself fails and the whole thing rolls back.
-- ----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION ledger_transaction_must_balance()
RETURNS TRIGGER AS $$
DECLARE
    imbalance RECORD;
BEGIN
    -- Find any currency within this transaction whose entries don't sum to 0.
    FOR imbalance IN
        SELECT currency, SUM(amount_minor) AS total
        FROM ledger_entries
        WHERE transaction_id = NEW.transaction_id
        GROUP BY currency
        HAVING SUM(amount_minor) <> 0
    LOOP
        RAISE EXCEPTION
            'ledger transaction % does not balance in %: sums to % (must be 0)',
            NEW.transaction_id, imbalance.currency, imbalance.total
            USING ERRCODE = 'integrity_constraint_violation';
    END LOOP;

    -- Also enforce the minimum-two-entries rule (double-entry, not single-sided).
    IF (SELECT COUNT(*) FROM ledger_entries WHERE transaction_id = NEW.transaction_id) < 2 THEN
        RAISE EXCEPTION
            'ledger transaction % has fewer than 2 entries', NEW.transaction_id
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER ledger_entries_balance_check
    AFTER INSERT ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ledger_transaction_must_balance();