-- zpay schema migration 0001: initial tables.
--
-- See ADR-0004 for the choice of libSQL and the schema discipline.
-- Every row carries a `network` column; per-network access is the norm.

CREATE TABLE IF NOT EXISTS zpay_schema_migrations (
    version       INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL,
    description   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS prepared_tx (
    payment_id                  TEXT    PRIMARY KEY,
    merchant_id                 TEXT    NOT NULL,
    network                     TEXT    NOT NULL CHECK (network IN ('mainnet', 'testnet', 'regtest')),
    recipient_unified_address   TEXT    NOT NULL,
    amount_zat                  INTEGER NOT NULL CHECK (amount_zat >= 0),
    memo_bytes                  BLOB    NOT NULL,
    expiry_height               INTEGER NOT NULL,
    agent_dpop_jkt              TEXT    NOT NULL,
    idempotency_key             TEXT    NOT NULL,
    created_at_unix_seconds     INTEGER NOT NULL,
    expires_at_unix_seconds     INTEGER NOT NULL,
    UNIQUE (merchant_id, agent_dpop_jkt, idempotency_key)
);

CREATE INDEX IF NOT EXISTS prepared_tx_expires_idx
    ON prepared_tx (expires_at_unix_seconds);

CREATE TABLE IF NOT EXISTS settlement_ledger (
    payment_id                                  TEXT    PRIMARY KEY,
    txid                                        TEXT    NOT NULL,
    network                                     TEXT    NOT NULL CHECK (network IN ('mainnet', 'testnet', 'regtest')),
    broadcast_at_unix_seconds                   INTEGER NOT NULL,
    broadcast_outcome                           TEXT    NOT NULL CHECK (broadcast_outcome IN ('accepted', 'duplicate', 'invalid_encoding', 'rejected', 'unknown')),
    current_confirmations_count                 INTEGER NOT NULL DEFAULT 0,
    last_confirmation_check_at_unix_seconds     INTEGER,
    evidence_pack_hash                          BLOB    NOT NULL,
    watch_id                                    TEXT    NOT NULL,
    FOREIGN KEY (payment_id) REFERENCES prepared_tx(payment_id)
);

CREATE INDEX IF NOT EXISTS settlement_ledger_txid_idx
    ON settlement_ledger (txid);

CREATE TABLE IF NOT EXISTS bearer_key_hash (
    key_hash                  BLOB    PRIMARY KEY,
    label                     TEXT    NOT NULL,
    created_at_unix_seconds   INTEGER NOT NULL,
    revoked_at_unix_seconds   INTEGER
);

INSERT INTO zpay_schema_migrations (version, applied_at_ms, description)
VALUES (1, (strftime('%s', 'now') * 1000), 'initial: prepared_tx, settlement_ledger, bearer_key_hash');
