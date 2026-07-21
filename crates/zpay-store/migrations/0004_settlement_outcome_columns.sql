-- zpay schema migration 0004: settlement outcome column invariant.
--
-- Accepted and Duplicate rows name the submitted transaction. Failure rows
-- carry an upstream diagnostic instead. Rebuilding the table makes that
-- enum-to-column contract durable for inserts and updates. The migration
-- runner rejects legacy violations before executing this script; values are
-- copied exactly and are never synthesized or rewritten.

CREATE TABLE settlement_ledger_v4 (
    payment_id                                  TEXT    PRIMARY KEY,
    broadcast_outcome_kind                      TEXT    NOT NULL CHECK (broadcast_outcome_kind IN ('accepted', 'duplicate', 'invalid_encoding', 'rejected', 'unknown')),
    transaction_id                              TEXT,
    upstream_message                            TEXT,
    settled_at_unix_seconds                     INTEGER NOT NULL,
    confirmation_count                          INTEGER,
    mined_block_height                          INTEGER,
    last_confirmation_check_at_unix_seconds     INTEGER,
    reorg_count                                 INTEGER NOT NULL DEFAULT 0,
    last_reorged_at                             INTEGER,
    expiry_height                               INTEGER,
    payee_id                                    TEXT    NOT NULL DEFAULT '',
    amount_zat                                  INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT settlement_outcome_columns_v4 CHECK (
        (
            broadcast_outcome_kind IN ('accepted', 'duplicate')
            AND transaction_id IS NOT NULL
            AND upstream_message IS NULL
        )
        OR
        (
            broadcast_outcome_kind IN ('invalid_encoding', 'rejected', 'unknown')
            AND transaction_id IS NULL
            AND upstream_message IS NOT NULL
        )
    )
);

INSERT INTO settlement_ledger_v4 (
    payment_id,
    broadcast_outcome_kind,
    transaction_id,
    upstream_message,
    settled_at_unix_seconds,
    confirmation_count,
    mined_block_height,
    last_confirmation_check_at_unix_seconds,
    reorg_count,
    last_reorged_at,
    expiry_height,
    payee_id,
    amount_zat
)
SELECT
    payment_id,
    broadcast_outcome_kind,
    transaction_id,
    upstream_message,
    settled_at_unix_seconds,
    confirmation_count,
    mined_block_height,
    last_confirmation_check_at_unix_seconds,
    reorg_count,
    last_reorged_at,
    expiry_height,
    payee_id,
    amount_zat
FROM settlement_ledger;

DROP TABLE settlement_ledger;
ALTER TABLE settlement_ledger_v4 RENAME TO settlement_ledger;

CREATE INDEX settlement_ledger_transaction_id_idx
    ON settlement_ledger (transaction_id)
    WHERE transaction_id IS NOT NULL;

CREATE INDEX settlement_ledger_mined_block_height_idx
    ON settlement_ledger (mined_block_height)
    WHERE mined_block_height IS NOT NULL;

CREATE INDEX settlement_ledger_payee_id_idx
    ON settlement_ledger (payee_id)
    WHERE payee_id != '';

INSERT INTO zpay_schema_migrations (version, applied_at_ms, description)
VALUES (4, (unixepoch() * 1000), 'settlement outcome columns: success requires transaction_id; failure requires upstream_message');
