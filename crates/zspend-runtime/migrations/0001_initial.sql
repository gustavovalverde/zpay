-- zspend schema migration 0001: single-use access-token jti ledger.
--
-- One row per access-token `jti`. `reserve` inserts a `pending` row before
-- signing; `commit` promotes it to `completed` with the stored signed payload
-- an identical replay returns. `reserved_at_ms` stamps the pending reservation
-- so an abandoned one can be reclaimed after the pending TTL.

CREATE TABLE IF NOT EXISTS zspend_schema_migrations (
    version       INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL,
    description   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS usage_ledger (
    jti            TEXT    PRIMARY KEY,
    intent_hash    TEXT    NOT NULL,
    state          TEXT    NOT NULL CHECK (state IN ('pending', 'completed')),
    response_json  TEXT,
    reserved_at_ms INTEGER NOT NULL
);

INSERT OR IGNORE INTO zspend_schema_migrations (version, applied_at_ms, description)
VALUES (1, (unixepoch() * 1000), 'initial: usage_ledger single-use jti reservations');
