# ADR-0004: libSQL for the prepared-tx cache and settlement ledger

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Persistence |
| Related | [ADR-0001](0001-workspace-and-crate-boundaries.md), [Operational surfaces](../architecture/operational-surfaces.md) |

## Context

zpay stores three classes of small typed records:

- **Prepared transactions** awaiting settlement, keyed by `(payment_id,
  agent_dpop_jkt)`. TTL bounded (5 minutes by default, 30 minutes max).
  Volume: thousands per merchant per day at most.
- **Settlement ledger** entries, append-only, keyed by `payment_id`.
  Volume: same as prepared transactions, persisted indefinitely.
- **Bearer key hashes**, the allowlist for programmatic clients (when
  enabled). Volume: tens to hundreds per deployment.

Candidates:

1. **In-memory (DashMap or parking_lot RwLock).** Lowest cost; no
   persistence across restart. Lost prepared transactions force the agent
   to re-prepare; unrecoverable settlements would require manual
   reconciliation against zinder.
2. **RocksDB.** zinder's choice. Optimised for high-throughput sequential
   writes of compact-block artifacts; over-engineered for zpay's
   typed-record workload.
3. **sqlx with vanilla SQLite.** Mature crate, broad ecosystem; no
   built-in Turso path for managed deployments.
4. **libsql.** Fauzec's choice. Drop-in SQLite locally, Turso in
   production, structured around per-connection Hrana streams. Same
   build-arch as fauzec's deployment.
5. **redis.** Lower latency for the cache; lossy for the ledger;
   introduces a second operational dependency.

The boundary between the prepared-tx cache (short-lived, mutable, TTL'd)
and the settlement ledger (long-lived, append-only) suggests two
backends might be appropriate. But every settlement ledger entry is
derived from a prepared-tx cache entry; splitting backends creates a
two-phase write and risks divergence.

## Decision

**libSQL for both the prepared-tx cache and the settlement ledger,
inside one zpay-store crate. SQLite file in local dev; Turso embedded
replica or remote Turso in production.**

Schema (initial migration `0001_initial.sql`):

- `prepared_tx (payment_id TEXT PRIMARY KEY, merchant_id TEXT, network
  TEXT, recipient_unified_address TEXT, amount_zat INTEGER, memo_bytes
  BLOB, expiry_height INTEGER, agent_dpop_jkt TEXT, idempotency_key
  TEXT, created_at_unix_seconds INTEGER, expires_at_unix_seconds INTEGER,
  UNIQUE (merchant_id, agent_dpop_jkt, idempotency_key))`
- `settlement_ledger (payment_id TEXT PRIMARY KEY, txid TEXT,
  network TEXT, broadcast_at_unix_seconds INTEGER, broadcast_outcome
  TEXT, current_confirmations INTEGER, last_confirmation_check_at_unix_seconds
  INTEGER, evidence_pack_hash BLOB, watch_id TEXT)` (append-only; rows
  are mutated only to update `current_confirmations` and
  `last_confirmation_check_at_unix_seconds`).
- `bearer_key_hash (key_hash BLOB PRIMARY KEY, label TEXT,
  created_at_unix_seconds INTEGER, revoked_at_unix_seconds INTEGER)`

Hand-rolled numbered migrations under `crates/zpay-store/migrations/`.
`SCHEMA_VERSION` constant and `MIGRATION_TABLE` name. Per-network schema
(every row carries `network`).

Connection management: a tiny `zpay-store::connection` module wrapping
libsql's connection pool with the same auto-reconnect discipline fauzec
uses for Hrana stream expiry.

## Rationale

libSQL is the right fit for zpay's workload shape: small typed records,
mixed read/write, TTL-based cleanup, append-only ledger, occasional
batch query. Fauzec's production usage of libSQL plus Turso is the
proven precedent. Two boundaries inside one store crate is simpler than
two crates with separate backends, and the unified transaction model
across `prepared_tx` and `settlement_ledger` is operationally important:
the broadcast-and-record step must be atomic.

## Consequences

Positive:

- One technology, one operational pattern. Local dev uses SQLite file;
  production uses Turso (or a local libsql replica). Same code path.
- Turso embedded replicas allow zpay to read locally and stream writes
  remotely, mitigating the network dependency for read-heavy operations.
- Transactions across `prepared_tx` and `settlement_ledger` are real
  SQL transactions, not coordinated writes across two stores.
- Migration discipline matches fauzec's hand-rolled numbered files; an
  operator can review every migration with one `git log` per file.

Negative:

- libsql is younger than vanilla SQLite or PostgreSQL; some sharp edges
  exist (Hrana stream expiry, dependency conflicts with rusqlite).
- A zpay deployment with no internet access (truly air-gapped) cannot
  use Turso and must fall back to local SQLite; documented in the
  runbook.

Neutral:

- The schema is intentionally minimal at scaffold time; new tables
  arrive with their own ADRs.

## Switch Criteria

Replace this decision when **any** of:

- Write throughput exceeds 10k events per minute sustained (libsql/Turso
  becomes the bottleneck; consider Postgres).
- An operator reports a hard incompatibility between libsql and a
  required deployment environment.
- A different sibling Rust service adopts a different DB and a shared
  store crate becomes valuable (rule of three).

## Alternatives Considered

### In-memory cache

Rejected. Lost prepared transactions force agents to re-prepare; lost
settlement ledger entries force manual reconciliation against zinder.
Even with persistence to a file on shutdown, the recovery path is more
complex than a single DB.

### RocksDB

Rejected. Optimised for sequential write throughput on compact-block
artifacts; not the right shape for typed records with transactional
boundaries. zinder uses RocksDB because zinder's workload is sequential
block-derivation writes; zpay's is not.

### Redis

Rejected. Loses the append-only ledger durability story; would force a
separate persistent backend for settlement, recreating the two-backend
divergence problem. The latency wins are not load-bearing for zpay's
prepare-then-settle pattern.

### Two-backend split (Redis cache + SQLite ledger)

Rejected. The atomicity of broadcast-and-record requires a unified
transaction. Splitting backends introduces a two-phase write that
becomes a debugging nightmare under partial failure.

## Out of Scope

- Migration to PostgreSQL or another RDBMS. Tracked in switch criteria.
- Multi-region replication beyond Turso's built-in replica model.
- Encryption at rest beyond the deployment's underlying disk encryption.
  Bearer keys are hashed; nothing else at rest is sensitive.
