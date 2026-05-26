-- zpay schema migration 0001: initial tables.
--
-- See ADR-0004 for the libSQL choice and the schema discipline. The
-- contract for each row mirrors the typed value in `zpay-core`
-- (`PreparedTxEntry`, `SettlementLedgerEntry`) exactly. Schema drift
-- between this file and the typed values is the failure mode the
-- migration test guards against.

CREATE TABLE IF NOT EXISTS zpay_schema_migrations (
    version       INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL,
    description   TEXT    NOT NULL
);

-- Prepared transactions awaiting settlement.
--
-- Idempotency: the `(merchant_id, idempotency_key)` pair is unique
-- when `idempotency_key` is non-null; a partial unique index enforces
-- this. The DPoP-bound key composite ADR-0004 originally specified
-- waits for the PRD-42 Phase 4 DPoP middleware to land in zpay-x402.
CREATE TABLE IF NOT EXISTS prepared_tx (
    payment_id                  TEXT    PRIMARY KEY,
    merchant_id                 TEXT    NOT NULL,
    network                     TEXT    NOT NULL CHECK (network IN ('mainnet', 'testnet', 'regtest')),
    recipient_unified_address   TEXT    NOT NULL,
    amount_zat                  INTEGER NOT NULL CHECK (amount_zat >= 0),
    memo_bytes                  BLOB    NOT NULL,
    expiry_height               INTEGER NOT NULL,
    idempotency_key             TEXT,
    expires_at_unix_seconds     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS prepared_tx_expires_idx
    ON prepared_tx (expires_at_unix_seconds);

CREATE UNIQUE INDEX IF NOT EXISTS prepared_tx_idempotency_idx
    ON prepared_tx (merchant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Settlement ledger, keyed by `payment_id`.
--
-- Every settle attempt records here, success or failure. Success-kind
-- outcomes (Accepted, Duplicate) carry a `transaction_id`; failure-kind
-- outcomes (Rejected, InvalidEncoding, Unknown, Queued) carry an
-- `upstream_message` instead. The confirmation oracle bumps
-- `confirmation_count`, `mined_block_height`, and
-- `last_confirmation_check_at_unix_seconds` over time.
CREATE TABLE IF NOT EXISTS settlement_ledger (
    payment_id                                  TEXT    PRIMARY KEY,
    broadcast_outcome_kind                      TEXT    NOT NULL CHECK (broadcast_outcome_kind IN ('accepted', 'duplicate', 'invalid_encoding', 'rejected', 'unknown')),
    transaction_id                              TEXT,
    upstream_message                            TEXT,
    settled_at_unix_seconds                     INTEGER NOT NULL,
    confirmation_count                          INTEGER,
    mined_block_height                          INTEGER,
    last_confirmation_check_at_unix_seconds     INTEGER
);

CREATE INDEX IF NOT EXISTS settlement_ledger_transaction_id_idx
    ON settlement_ledger (transaction_id)
    WHERE transaction_id IS NOT NULL;

INSERT OR IGNORE INTO zpay_schema_migrations (version, applied_at_ms, description)
VALUES (1, (unixepoch() * 1000), 'initial: prepared_tx, settlement_ledger');
