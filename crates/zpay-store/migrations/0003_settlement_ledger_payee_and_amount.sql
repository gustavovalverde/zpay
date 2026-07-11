-- zpay schema migration 0003: payee and amount attribution on the
-- settlement ledger, for the operator payments console (see ADR-0014).
--
-- `prepared_tx` carries `payee_id` and `amount_zat`, but its row is
-- deleted on successful settle (fire-once idempotency). Without these
-- columns, a settled payment's payee and amount are gone from persistent
-- storage the moment it settles. Existing rows predate this migration and
-- get the sentinel defaults below; they are excluded from payee-filtered
-- queries.

ALTER TABLE settlement_ledger ADD COLUMN payee_id TEXT NOT NULL DEFAULT '';
ALTER TABLE settlement_ledger ADD COLUMN amount_zat INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS settlement_ledger_payee_id_idx
    ON settlement_ledger (payee_id)
    WHERE payee_id != '';

INSERT OR IGNORE INTO zpay_schema_migrations (version, applied_at_ms, description)
VALUES (3, (unixepoch() * 1000), 'settlement_ledger: payee_id, amount_zat for the operator payments console (ADR-0014)');
