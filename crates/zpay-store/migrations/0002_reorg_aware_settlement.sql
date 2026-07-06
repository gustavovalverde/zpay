-- zpay schema migration 0002: reorg-aware settlement ledger.
--
-- The settlement lifecycle downgrades a mined payment back to Broadcast
-- when a reorg drops its block. `reorg_count` and `last_reorged_at`
-- record that regression. `expiry_height` carries the prepared expiry
-- onto the ledger so the status projection can lapse an unmined row to
-- Expired after the success path removes the prepared row.

ALTER TABLE settlement_ledger ADD COLUMN reorg_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE settlement_ledger ADD COLUMN last_reorged_at INTEGER;
ALTER TABLE settlement_ledger ADD COLUMN expiry_height INTEGER;

CREATE INDEX IF NOT EXISTS settlement_ledger_mined_block_height_idx
    ON settlement_ledger (mined_block_height)
    WHERE mined_block_height IS NOT NULL;

INSERT OR IGNORE INTO zpay_schema_migrations (version, applied_at_ms, description)
VALUES (2, (unixepoch() * 1000), 'reorg-aware settlement: reorg_count, last_reorged_at, expiry_height');
