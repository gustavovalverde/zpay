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

Shipped schema (after the 2026-05-26 reconciliation and the 2026-06-03
DPoP-binding migration; see the revision history for the redesigns
that landed each column):

- `prepared_tx (payment_id TEXT PRIMARY KEY, payee_id TEXT NOT NULL,
  network TEXT NOT NULL, scheme TEXT NOT NULL, recipient_unified_address
  TEXT NOT NULL, amount_zat INTEGER NOT NULL, payment_uri TEXT NOT NULL,
  memo_bytes BLOB NOT NULL, expiry_height INTEGER NOT NULL,
  agent_dpop_jkt TEXT NOT NULL, idempotency_key TEXT, intent_posture
  TEXT NOT NULL, merchant_requires_verify INTEGER NOT NULL,
  created_at_unix_seconds INTEGER NOT NULL, expires_at_unix_seconds
  INTEGER NOT NULL)` plus a partial unique index
  `prepared_tx_idempotency_idx ON prepared_tx (agent_dpop_jkt,
  idempotency_key) WHERE idempotency_key IS NOT NULL`. The composite
  is the idempotency identity: a `(jkt, idempotency_key)` pair replays
  to the same `payment_id`, but a different `jkt` reusing the same
  `idempotency_key` allocates a fresh row, which is the property the
  DPoP middleware buys.
- `settlement_ledger (payment_id TEXT PRIMARY KEY, broadcast_outcome_kind
  TEXT NOT NULL, transaction_id TEXT, upstream_message TEXT,
  settled_at_unix_seconds INTEGER NOT NULL, confirmation_count INTEGER,
  mined_block_height INTEGER, last_confirmation_check_at_unix_seconds
  INTEGER)`. Append-only; rows are mutated only by the confirmation
  oracle as it updates `confirmation_count`, `mined_block_height`, and
  `last_confirmation_check_at_unix_seconds`.
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

## Revision history

### 2026-05-26: Schema 0001 reconciliation

The 0001 migration shipped now matches the typed values in zpay-core
exactly. Three adjustments against the scaffold:

- **Drop `agent_dpop_jkt` from `prepared_tx`.** DPoP-bound idempotency
  is PRD-42 Phase 4 work that has not landed yet, so requiring a NOT
  NULL column for a value zpay does not produce would force callers
  to invent a placeholder. The idempotency uniqueness narrows to
  `(merchant_id, idempotency_key)` enforced by a partial UNIQUE
  index (`prepared_tx_idempotency_idx WHERE idempotency_key IS NOT
  NULL`). The DPoP join key arrives with its own migration when the
  Phase 4 middleware lands.
- **`settlement_ledger` carries `broadcast_outcome_kind`,
  `transaction_id` (nullable), `upstream_message` (nullable),
  `confirmation_count` (nullable), `mined_block_height` (nullable),
  `last_confirmation_check_at_unix_seconds` (nullable).** The
  flattened triple replaces `txid NOT NULL` because failure-kind
  outcomes (`Rejected`, `InvalidEncoding`, `Unknown`) do not carry a
  txid. `mined_block_height` and the per-confirmation timestamp are
  the columns the confirmation oracle updates on every tick.
- **Defer `evidence_pack_hash` and `watch_id` columns on
  `settlement_ledger`.** Neither is produced by today's broadcast or
  oracle paths; both follow when PRD-42 Phase 6 (`evidence_pack_hash`
  delivery from the MCP bridge) and Phase 4 (`watch_id` for per-txid
  push subscriptions) ship.

The migration runner in `zpay-store::migration` applies these scripts
through `execute_transactional_batch` and tracks state in
`zpay_schema_migrations`. The runtime composes between an in-memory
`PreparedTxCache` / `SettlementLedger` (via `zpay-core`'s `in_memory`
feature, default-on for tests) and the libSQL implementations via
the `ZPAY_STORE__BACKEND` env var (`memory` or `libsql`; defaults to
`libsql` with `ZPAY_STORE__URL` defaulting to `file:./zpay.libsql`).

### 2026-06-03: DPoP-bound idempotency composite ships

Commit E lands the `(agent_dpop_jkt, idempotency_key)` composite the
2026-05-26 entry deferred. `prepared_tx.agent_dpop_jkt` is `TEXT NOT
NULL`; the partial UNIQUE INDEX `prepared_tx_idempotency_idx` switches
from `(payee_id, idempotency_key)` to `(agent_dpop_jkt,
idempotency_key) WHERE idempotency_key IS NOT NULL`. A new DPoP
middleware in `zpay-x402::dpop` verifies an ES256 proof on every
`POST /x402/v2/prepare` and `POST /x402/v2/settle` request, extracts
the RFC 7638 JWK thumbprint, and threads it through `propose` and
`submit_settlement`. Settle compares the verified jkt against the
prepared row and refuses any mismatch with 403 `dpop_mismatch`. The
`/accepts`, `/tip`, `/payments/{id}`, `/payments/{id}/events`, and
`/verify` routes stay unauthenticated; payment-id is the capability
that gates them.

The replay store lives in `AppState` and keys `(jkt, jti)` for a
5-minute window. Clock skew tolerance is +/- 60 seconds. Each
typed `DpopError` variant maps to a unique `application/problem+json`
code (`dpop_missing`, `dpop_invalid_proof`, `dpop_clock_skew`,
`dpop_replay`, `dpop_mismatch`) so wire consumers can branch on the
failure mode without sniffing the prose detail.

### 2026-06-03: Commit G DPoP production hardening

Commit G tightens the verifier into a shape production deployments can
ship as-is and extends the schema's idempotency story into a wire
contract operators control via env vars. Seven changes land together:

- **Case-sensitive `htm`.** RFC 9449 requires byte-exact matching on
  the HTTP method. The verifier no longer folds case; the demo BFF
  sends upper-case verbs verbatim. A new unit test
  (`htm_lower_case_rejected_rfc9449`) is the regression gate.
- **Pinned host and scheme.** `ZPAY_EXPECTED_HOST` and
  `ZPAY_EXPECTED_SCHEME` pin the canonical request URL the verifier
  compares against. When unset the runtime falls back to the inbound
  `Host` header and emits a startup `WARN`, so an attacker sending
  `Host: evil.com` alongside a proof minted against `evil.com` cannot
  trick the verifier in a production deployment that has pinned the
  host.
- **`jti` length cap.** A new `MAX_JTI_LEN = 128` constant bounds the
  memory an adversary can pin per `(jkt, jti)` row.
- **`jti` burned on failed proofs.** The replay-store `observe` now
  runs BEFORE htu/htm/iat/signature validation. A probe-then-replay
  attack window (mint with wrong `htm` to learn the verifier's
  response, then retry with a corrected `htm` and the same `jti`) no
  longer exists.
- **Structural URL canonicalization.** `canonicalize_url` parses both
  sides through the `url` crate, resolving dot segments, lowercasing
  the host, stripping default ports, and normalizing percent-encoding.
  Encoded slashes (`%2f`) remain distinct from literal slashes so
  path-traversal attempts that tunnel through an encoded slash do not
  collapse onto the same canonical path.
- **`ReplayStore` trait.** The replay store becomes
  `Arc<dyn ReplayStore>` carried on `AppState`. Production deployments
  swap a shared Redis or libSQL backend without touching the verifier
  or the wire handlers. The bundled `InMemoryReplayStore` stays the
  default for single-process runs.
- **Deterministic BFF keypair.** The demo BFF now derives its ES256
  keypair from `ZPAY_DPOP_KEY_SEED` via HKDF-SHA-256, so the
  `(jkt, idempotency_key)` composite stays stable across BFF restarts
  and serverless cold starts. When the seed is unset the helper falls
  back to the previous ephemeral generation and emits a `console.warn`.

The schema does not change in Commit G; the production hardening is a
verifier discipline, not a persistence change. The `agent_dpop_jkt`
column and the `(agent_dpop_jkt, idempotency_key)` partial UNIQUE
index documented above remain the storage shape.

### 2026-05-26: Persistence harness without a chain plane

`scripts/test-persistence.sh` exercises the libSQL surface end-to-end
without zinder, fauzec, or zentity. It builds the release runtime,
spins it up on a tempdir-scoped libSQL file, and asserts three
invariants:

1. A `/prepare` call writes a row that survives a process kill (a
   second `GET /payments/{id}` after restart returns the same status
   and payment_id).
2. The partial-UNIQUE idempotency index on
   `(merchant_id, idempotency_key)` survives restart (replay with
   the same key still resolves to the original payment_id, not a
   freshly allocated one).
3. Distinct idempotency keys remain distinct payment_ids across the
   same restart (no accidental collapse onto a single row).

The script is the regression gate for the persistence slice. Run it
before any change to `zpay-store`, the migrations, or the
`PreparedTxStore` / `SettlementLedgerStore` traits.

### 2026-07-05: Schema version 2 (reorg-aware ledger); no bearer table; separate zspend ledger

Migration `0002_reorg_aware_settlement.sql` takes the schema to version 2.
It adds three columns to `settlement_ledger`: `reorg_count INTEGER NOT NULL
DEFAULT 0`, `last_reorged_at INTEGER` (nullable), and `expiry_height
INTEGER` (nullable), plus a partial index on `mined_block_height` where it
is non-null. `reorg_count` and `last_reorged_at` record a payment's
regression when a reorg drops its block; `expiry_height` carries the
prepared expiry onto the ledger so an unmined row can lapse to `Expired`
after the success path removes its prepared row. These columns back the
settlement lifecycle in
[ADR-0009](0009-settlement-lifecycle-and-finality.md).

There is no `bearer_key_hash` table. Schema version 1 creates exactly
`zpay_schema_migrations`, `prepared_tx`, and `settlement_ledger`; the
scaffold's bearer-key allowlist never shipped, and zpay exposes no
bearer-key surface. The Context and Decision references to a
`bearer_key_hash` table are superseded by this entry.

The wallet runtime (`zspend-runtime`) owns a separate single-file libSQL
database for its single-use `jti` ledger, not part of the `zpay-store`
schema. It is `usage_ledger`, addressed by `ZSPEND_LEDGER_URL` (default a
`usage-ledger.db` file beside `ZSPEND_STORAGE_PATH`), with its own
migration at `crates/zspend-runtime/migrations/0001_initial.sql`. zpay's
store and the wallet's ledger share neither a schema nor a connection.

A `libsql://` remote URL requires `ZSPEND_LEDGER_AUTH_TOKEN`; startup fails
closed with a typed error rather than opening an unauthenticated connection
when the token is missing. A file-backed URL ignores the token.
