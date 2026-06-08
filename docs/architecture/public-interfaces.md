# Public Interfaces

This document is the vocabulary spine. Every architecture doc, ADR, RPC
method, error variant, configuration field, database column, capability
string, and identifier defers to the conventions here.

The names chosen here will be copied by contributors, downstream wallets,
operators, and code-extending agents. They will live longer than the code
that introduces them. Treat the spine as the canonical reference.

Optimization order:

1. Developer Experience (humans writing or reviewing zpay code).
2. Agent Experience (machines calling zpay over HTTP).
3. User Experience (humans approving payments in their wallets).
4. Contributor Experience (humans extending zpay).

When the order conflicts, DX wins.

## Vocabulary

### Product and runtimes

| Term | Meaning |
|------|---------|
| `zpay` | The product, the workspace, the brand. |
| `zpay-core` | Library crate: domain types, prepare, oracle, broadcast, compliance, capability. |
| `zpay-store` | Library crate: libSQL prepared-tx cache, settlement ledger, bearer-key-hash table. |
| `zpay-x402` | Library crate: x402 v2 wire adapter. |
| `zpay-mpp` | Library crate: MPP wire adapter. Feature-gated; off by default. |
| `zpay-runtime` | Binary: composition root, Axum HTTP server, ops listener, env-driven config. |
| `zpay-testkit` | Library crate (test-only): live-test gates, mocks, fixtures. |
| Facilitator | The role zpay plays in a payment: prepare, hold, broadcast, confirm. |
| Adapter | The wire-protocol translation layer (`zpay-x402`, `zpay-mpp`). |
| Operator | The human running a zpay deployment. |
| Agent | The machine calling zpay's HTTP surface. |
| Merchant | The party accepting a payment, identified by `merchant_id`. |
| Payer | The human whose wallet signs the unbroadcast transaction. |

### Domain types

| Type | Meaning |
|------|---------|
| `Network` | `Mainnet`, `Testnet`, `Regtest`. Carried by every public type. |
| `Zatoshis` | `u64` newtype; non-negative integer zatoshis. |
| `Zec` | Human-readable decimal string. Internal computation uses `Zatoshis`. |
| `TxId` | Newtype over `[u8; 32]` for v5 ZIP-244 txid. Network-tagged. |
| `BlockHeight` | `u32` newtype. |
| `UnifiedAddress` | ZIP-316 unified address; network-tagged at construction. |
| `Memo` | Up to 512 bytes per ZIP-302; carries a network tag and a protocol-version byte. |
| `IdempotencyKey` | Caller-supplied opaque string; zpay re-uses zally's primitive. |
| `PaymentId` | Server-issued ULID-shaped identifier; pairs with a `dpop_jkt`. |
| `Preparation` | The result of `core::prepare::propose`: `{ payment_id, payment_uri, memo, expiry_height }`. |
| `SettlementOutcome` | The result of `core::broadcast::submit`: `{ txid, broadcast_outcome, watch_id }`. |
| `ConfirmationStatus` | `{ status, confirmations, block_height }` returned from the oracle. |
| `PohToken` | A zentity-issued SD-JWT-VC carrying derived claims. |
| `PohClaims` | `{ verification_level, verified, sybil_resistant, merchant_sub, aud, cnf_jkt, exp }`. |
| `MerchantId` | Operator-assigned identifier (lowercase kebab-case). |
| `WatchId` | Identifier returned by the confirmation oracle for a per-txid subscription. |
| `EvidencePackHash` | 32-byte SHA-256 over zentity's `(policy_hash, proof_set_hash)` pair. |

### Wire surface (HTTP)

`/x402/v2/*` and `/mpp/v1/*` (when feature-enabled) are the agent surfaces.
Both follow the same lifecycle:

| Step | x402 v2 path | MPP path | Capability |
|------|--------------|----------|------------|
| Advertise | `GET /x402/v2/accepts?merchant=…&resource=…` | `GET /mpp/v1/accepts` | `*.accepts` |
| Prepare | `POST /x402/v2/prepare` | `POST /mpp/v1/prepare` | `*.prepare` |
| Settle | `POST /x402/v2/settle` | `POST /mpp/v1/settle` | `*.settle` |
| Verify | `POST /x402/v2/verify` | `POST /mpp/v1/verify` | `*.verify` |
| Status | `GET /x402/v2/payments/{payment_id}` | `GET /mpp/v1/payments/{payment_id}` | `*.payments` |

Operational surface:

| Path | Purpose |
|------|---------|
| `GET /healthz` | Process liveness (200 if the process is running). |
| `GET /readyz` | Dependency readiness (200 if zinder and libSQL reachable, else 503). |
| `GET /metrics` | Prometheus text format. |
| `GET /openapi.json` | Machine-readable OpenAPI 3.1 wire contract. |

### Wire surface (gRPC)

zpay does not expose a gRPC server in v1. It consumes zinder's gRPC via
`zinder-client::RemoteChainIndex`. A zpay control-plane gRPC may appear in
a future ADR when (and only when) a concrete consumer asks.

### Capability strings

Capability strings follow `<surface>.<adapter-version>.<verb>` and a
trailing version suffix where the wire shape evolves:

```text
x402.v2.accepts
x402.v2.prepare
x402.v2.settle
x402.v2.verify
x402.v2.payments
mpp.v1.accepts
mpp.v1.prepare
mpp.v1.settle
mpp.v1.verify
mpp.v1.payments
broadcast.transaction.v1
broadcast.oracle.confirm_v1
cache.prepare.idempotent
cache.prepare.ttl
cache.settlement.ledger
compliance.poh.verify_v1
compliance.poh.pairwise_v1
compliance.evidence.bind_v1
```

Capability strings appear in:

- The `capabilities[]` array on every wire response.
- `/healthz` body (so an operator script can grep for them).
- The OpenAPI spec's `x-capability` extension on each operation.

A capability that is "advertised but not yet enabled" returns 503 with
`Reason::CapabilityUnavailable`. Operators turn capabilities on in
configuration, not in code.

## Naming rules

### Forbidden roots

Anywhere in any identifier:

`bar`, `common`, `data`, `foo`, `handler`, `helpers`, `info`, `item`,
`manager`, `obj`, `payload`, `processor`, `result`, `shared`, `stuff`,
`thing`, `tmp`, `utils`, `value`.

As suffixes:

`*Api`, `*Data`, `*Helper`, `*Info`, `*Manager`, `*Processor`, `*Server`,
`*Service`, `*Util`.

If a module or symbol cannot be named by domain, the boundary is not
understood.

### Required suffixes

- Duration: `_ms`, `_seconds`, `_minutes`, `_hours`, `_blocks`, `_height`.
  Never bare `timeout`, `delay`, `interval`, `expires`.
- Money: `_zat` for integer zatoshis, `_zec` for decimal-string ZEC.
  Never bare `amount`.
- Booleans: `is_*`, `has_*`, `can_*`. Affirmative only.
- Counts: `_count`.
- Bytes: `_bytes`.

### Network-tagged everywhere

Every public type that names an address, key, balance, or transaction
carries a `Network` value. Constructors fail closed on mismatch. A
function that takes an address but not a network is a review-blocking
smell.

### Verbs from a single vocabulary

`accept`, `advertise`, `broadcast`, `compute`, `confirm`, `derive`,
`discover`, `find`, `get`, `observe`, `parse`, `prepare`, `prove`,
`settle`, `sign`, `submit`, `verify`, `watch`.

Forbidden for domain operations: `do`, `execute`, `handle`, `manage`,
`perform`, `process`.

### No temporal or implementation drift in names

`new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `redis_x`,
`libsql_x`, `axum_x` are banned. The name of a thing must survive a
change of its implementation.

## Type conventions

- Newtypes for anything carrying a unit (zatoshis, blocks, ms). The unit
  lives in the type, not the variable name.
- `serde::{Serialize, Deserialize}` on every type that crosses the wire
  or a DB boundary.
- `#[non_exhaustive]` on every public enum that might grow.
- `#[serde(rename_all = "snake_case")]` on enums that cross the wire.
- `Result<T, E>` always; `Option<T>` only when absence is semantically
  meaningful.
- `thiserror::Error` for every error type; one error enum per crate's
  public surface.

## Error vocabulary

`thiserror` v2 throughout. Each variant has a documented retry posture:

- **`retryable`**: caller may retry without changing anything.
- **`not_retryable`**: caller must change inputs to succeed.
- **`requires_operator`**: zpay operator must act before this can succeed.

Full registry in [docs/reference/error-vocabulary.md](../reference/error-vocabulary.md).

At wire boundaries, typed errors map to HTTP status codes via a single
`into_problem()` impl. Adapters never expose `tonic::Status`,
`reqwest::Error`, or `libsql::Error` upward.

## Config and env var conventions

- Env var prefix: `ZPAY_*`. Nested fields use `__` separator.
  Example: `ZPAY_CHAIN_SOURCE_URL`.
- Test-only env vars: `ZPAY_TEST_*`. Production binaries strip them.
- Live-node gate: `ZPAY_TEST_LIVE=1`. Mainnet allowance: `ZPAY_TEST_ALLOW_MAINNET=1`.
- Sensitive leaves never set via env var alone; they come from a secret
  manager and `--print-config` redacts them as `[REDACTED]`.

Top-level config sections:

| Section | Purpose |
|---------|---------|
| `[server]` | HTTP bind address, TLS, CORS allowlist, request limits. |
| `[node]` | zinder gRPC endpoint, fallback zexplorer REST endpoint. |
| `[wallet]` | Operator wallet seed sealing (age identity, network). |
| `[store]` | libSQL connection URL, replica config, schema-migration policy. |
| `[compliance]` | zentity JWKS URL, cache TTL, accepted issuers. |
| `[merchants]` | Per-merchant `accepts[]` template. Loaded from TOML; hot-reload on SIGHUP. |
| `[ops]` | Ops listener bind address, metrics namespace. |
| `[telemetry]` | Tracing format, log filter, sampling rate. |

## ZIP and spec compliance surface

### Implemented (M0 scaffold target)

- ZIP-316 unified addresses (recognised; full parsing via zally).
- ZIP-302 memos (constructed via zally's `Memo::from_bytes`).
- ZIP-225 + ZIP-244 v5 transactions and txids (everything zpay touches
  is v5).
- ZIP-321 payment URIs (parsed and emitted via zally).
- x402 v2 wire protocol (the entire `/x402/v2/*` surface).

### Reserved by shape (Phase 4-6 targets)

- ZIP-311 payment disclosures (verifier inside zinder; zpay delegates).
- ZIP-317 conventional fees (delegated to zally's proposal builder).
- MPP wire protocol (`/mpp/v1/*` surface; feature-gated).
- SD-JWT-VC PoH validation (EdDSA against zentity's JWKS).

### Out of scope

- ZIP-32 (HD derivation): zally owns this.
- ZIP-304 (Sapling address signatures): not used by zpay's surface.
- ZIP-310 (viewing key properties): zentity owns this.
- ZIP-312 (FROST for Zcash): not used by zpay's facilitator wallet.
- ZIP-325 (account metadata keys): zentity owns this.
- v4 transactions: deliberately not supported.

## Cross-references

- [Operational surfaces](operational-surfaces.md): readiness state machine,
  ops port, live-test gates.
- [Facilitator plane](facilitator-plane.md): prepare, settle, watch, verify
  lifecycle and typed errors at each boundary.
- [Upstream platform binding](upstream-platform-binding.md): what zpay
  expects from zally, zinder, zexplorer, and zentity.
- [Error vocabulary](../reference/error-vocabulary.md): every typed error
  with retry posture.
- [ADR index](../README.md): locked architectural decisions.
