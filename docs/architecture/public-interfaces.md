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
| `zpay-core` | Library crate: protocol-neutral domain types, prepare, oracle, broadcast, status projection, local ZIP-311 verifier, capability registry. |
| `zpay-dpop` | Library crate: pure RFC 7638 JWK thumbprint and RFC 9449 `htu` canonicalization used by the facilitator and wallet DPoP verifiers. |
| `zpay-store` | Library crate: libSQL prepared-tx cache and settlement ledger. |
| `zpay-x402` | Library crate: official x402 v2 facilitator routes, x402 header codecs, and the zpay lifecycle router used by demos and harnesses. |
| `zpay-runtime` | Binary: composition root, Axum HTTP server, ops listener, env-driven config. |
| `zpay-e2e` | Binary: end-to-end testnet validator that drives the full lifecycle against a running `zpay-runtime` plus zinder. |
| `zpay-demo` | Dev-only binary: local browser checkout gateway. It owns demo wallet state and demo credentials, then calls zpay and zspend through their existing HTTP surfaces. |
| `zpay-testkit` | Dev and test library: DPoP-bound agent payment client fixtures used by `zpay-demo` and `zpay-e2e`. |
| `zspend-core` | Library crate: service-internal wallet auth types (`payment_authorization` RAR, PRC-7807 problem details, signing policy). |
| `zspend-runtime` | Binary: the agent-bound wallet that signs under a bounded grant. zpay stays broadcaster-only. See [Proposal-0003](../proposals/0003-agent-wallet-production-architecture.md). |
| Facilitator | The role zpay plays in a payment: verify, settle, and report supported payment kinds at the official x402 boundary. |
| Adapter | The wire-protocol translation layer (`zpay-x402`). |
| Operator | The human running a zpay deployment. |
| Agent | The machine calling zpay's HTTP surface. |
| Payee | The party accepting a payment, identified by `payee_id`. |
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
| `Preparation` | The result of `core::prepare::propose`: `{ payment_id, payment_uri, memo_bytes, expiry_height, amount_zat }`. |
| `SettlementOutcome` | The result of `core::settle::submit_settlement`: `{ payment_id, broadcast_outcome, watch_id }`. |
| `PaymentStatusSnapshot` | Lifecycle projection returned by the status route and the SSE stream: `{ payment_id, status, intent_posture, broadcast_outcome, settled_at_unix_seconds, confirmation_count, mined_block_height, reorg_count, settled }`. |
| `PaymentStatus` | `awaiting`, `broadcast`, `mined`, `final`, `failed`, `never_issued`, `expired`. Regression-capable; see [ADR-0009](../adrs/0009-settlement-lifecycle-and-finality.md). |
| `PayeeId` | Operator-assigned identifier for the party accepting a payment. |
| `WatchId` | Identifier returned by the confirmation oracle for a per-txid subscription. |
| `EvidencePackHash` | 32-byte SHA-256 over zentity's `(policy_hash, proof_set_hash)` pair. |

### Wire surface (HTTP)

`/x402/v2/*` is the official x402 facilitator surface. It follows the x402
v2 facilitator API, not zpay's Zcash prepare lifecycle:

| Step | Path | Method |
|------|------|--------|
| Supported kinds | `/x402/v2/supported` | GET |
| Verify | `/x402/v2/verify` | POST |
| Settle | `/x402/v2/settle` | POST |

`GET /x402/v2/supported` advertises the configured Zcash `exact` payment kind.
The binding is `x402-zcash-exact-v1`; the official `verify` and `settle`
endpoints verify, extract, and broadcast `pczt-v2-extractable` PCZT
authorizations. When `PaymentRequirements.extra.zpayPaymentId` is present,
`settle` also records the broadcast outcome against that prepared lifecycle row
after proving the row's amount, recipient, network, and expiry match the PCZT
requirements. See
[ADR-0010](../adrs/0010-x402-public-boundary.md) and
[ADR-0011](../adrs/0011-zcash-x402-exact-binding.md).

`/zpay/v1/*` is zpay's product lifecycle surface. It is not an x402
compatibility surface:

| Step | Path | Method |
|------|------|--------|
| Advertise | `/zpay/v1/accepts?payee_id=…` | GET |
| Chain tip | `/zpay/v1/tip` | GET |
| Prepare | `/zpay/v1/prepare` | POST |
| Settle | `/zpay/v1/settle` | POST |
| Verify disclosure | `/zpay/v1/verify` | POST |
| Status | `/zpay/v1/payments/{payment_id}` | GET |
| Status stream | `/zpay/v1/payments/{payment_id}/events` | GET (SSE) |

The party accepting a payment is a **payee**, identified by `payee_id`; the
wire field and the registry key are `payee_id`. No MPP surface is mounted and
no OpenAPI document is served.

`GET /zpay/v1/payments/{payment_id}` and the SSE snapshot carry the
settlement lifecycle fields, including `reorg_count` (how many times a reorg
returned the payment from a mined status to broadcast) and `settled` (true
once the payment is at or below the chain's settled tip and no reorg can move
it). See [ADR-0009](../adrs/0009-settlement-lifecycle-and-finality.md).

The SSE stream closes after the first snapshot for which `settled` is true or
the status is terminal (`failed`, `never_issued`, `expired`). A `final`
snapshot that is not yet settled keeps the stream open, so a later reorg
downgrade still reaches the subscriber.

**Rate limiting.** DPoP-authenticated routes are limited per `jkt`,
unauthenticated routes per client IP, each on a fixed 60-second window
(`ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE` default 120,
`ZPAY_RATE_LIMIT__PER_IP_PER_MINUTE` default 600; `0` disables a dimension).
A limited request returns 429 with `Retry-After` and the problem envelope.

**CORS.** `ZPAY_SERVER__CORS__ALLOWLIST` is a comma-separated list of exact
origins; empty or unset emits no CORS headers.

Operational surface (ops listener):

| Path | Purpose |
|------|---------|
| `GET /healthz` | Process liveness, `{"status":"alive"}`. Also mounted on the main listener. |
| `GET /readyz` | Dependency readiness (chain plane plus store), 200 or 503. |
| `GET /metrics` | Prometheus text format. |
| `GET /payments` | Operator payments console (see [ADR-0014](../adrs/0014-operator-payments-console.md)). |

Dev demo surface (`zpay-demo`, loopback only by default):

| Path | Purpose |
|------|---------|
| `GET /demo/v1/readiness` | Readiness projection for zpay, zspend, zinder, the demo wallet, fauzec, and the network. |
| `GET /demo/v1/wallet` | Demo wallet address, balances, funding posture, and network. |
| `POST /demo/v1/faucet-claims` | Optional fauzec claim for first-run funding. |
| `GET /demo/v1/faucet-claims/{request_id}` | Fauzec claim status. |
| `GET /demo/v1/payments` | List payments made this session, most recent first. |
| `POST /demo/v1/payments` | Prepare a zpay checkout in `checkout` or `autopay` mode. |
| `POST /demo/v1/payments/{payment_id}/settle` | Sign and settle the prepared demo payment. |
| `GET /demo/v1/payments/{payment_id}` | Enriched demo payment status. |
| `GET /demo/v1/payments/{payment_id}/events` | SSE stream of enriched demo payment status. |
| `POST /demo/v1/verify` | Proxy to zpay's ZIP-311 disclosure verify. |
| `GET /demo/v1/console/payments` | Proxy to zpay's ops-listener operator console. |

The demo route surface is not a production zpay runtime API. Browser clients
call only this gateway; the gateway holds demo-only wallet state, DPoP keys,
issuer keys, access tokens, and signed transaction bytes.

### Wire surface (gRPC)

zpay does not expose a gRPC server in v1. It consumes zinder's gRPC via
`zinder-client::RemoteChainIndex`. A zpay control-plane gRPC may appear in
a future ADR when (and only when) a concrete consumer asks.

### Capability strings

Capability strings follow `<surface>.<adapter-version>.<verb>` and a
trailing version suffix where the wire shape evolves:

```text
x402.v2.supported
x402.v2.verify
x402.v2.settle
zpay.v1.accepts
zpay.v1.prepare
zpay.v1.settle
zpay.v1.verify
zpay.v1.payments
broadcast.transaction.v1
broadcast.oracle.confirm_v1
cache.prepare.idempotent
cache.prepare.ttl
cache.settlement.ledger
```

These strings are a naming registry in `zpay-core::capability`, defined for
the discipline they encode; they are not yet emitted on wire responses. zpay
advertises no compliance capability: spend-policy authority for the
agent-signed path lives in the identity issuer, and zpay runs no PoH gate
(see [ADR-0008](../adrs/0008-compliance-authority-placement.md)).

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

Standards-owned wire terms are the only exception. `zpay-x402::wire` may use
exact x402 names such as `PaymentPayload`, `paymentPayload`, and `payload` so
the serialized JSON matches the standard. Those names must not leak into
protocol-neutral `zpay-core` identifiers.

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

- Env var prefix: `ZPAY_*`. Nested fields use `__` as the separator.
  Example: `ZPAY_CHAIN_SOURCE_URL`, `ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE`.
- The runtime is configured entirely by environment variables. The only file
  input is the payee registry (`ZPAY_PAYEES__CONFIG_PATH`), a TOML file of
  `[payees.<id>]` entries carrying each payee's `accepts[]` template; it is
  read once at startup, not hot-reloaded.
- The one secret the runtime reads is the Turso auth token
  (`ZPAY_STORE__AUTH_TOKEN`); `--print-config` redacts it as `[REDACTED]`.
- The full env-var schema lives in
  [operational-surfaces.md](operational-surfaces.md).

## ZIP and spec compliance surface

### Implemented

- ZIP-316 unified addresses (recognised; full parsing via zally).
- ZIP-302 memos (constructed via zally's `Memo::from_bytes`).
- ZIP-225 + ZIP-244 transactions and txids. The `/settle` expiry gate parses
  v5 and v6 (NU6.3/Ironwood) through zally; see
  [ADR-0006](../adrs/0006-facilitator-trust-boundary.md).
- ZIP-321 payment URIs (parsed and emitted via zally).
- ZIP-311 payment disclosures (local verifier in `zpay-core`; see
  [ADR-0007](../adrs/0007-local-zip311-verifier.md)).
- x402 v2 facilitator envelope: `/supported`, `/verify`, `/settle`, official
  JSON field names, and `PAYMENT-REQUIRED`, `PAYMENT-SIGNATURE`,
  `PAYMENT-RESPONSE` header codecs.
- Zcash x402 `exact` binding:
  `x402-zcash-exact-v1`, `scheme: "exact"`, `network:
  "zcash:mainnet" | "zcash:testnet" | "zcash:regtest"`, `asset: "ZEC"`,
  integer zatoshi `amount`, ZIP-316 Unified Address `payTo`, and
  `payload.format: "pczt-v2-extractable"`. The x402 route verifies labelled
  shielded PCZT payment effects before extraction and broadcast.

### Deferred

- ZIP-317 conventional fees (delegated to zally's proposal builder).
- Zcash x402 `exact` PCZT settlement: parse ZIP-374 PCZT bytes, prove recipient
  and amount semantics before broadcast, extract the transaction, and return
  the txid through the official x402 settlement response. No Zcash kind is
  advertised through `/x402/v2/supported` until this lands.
- SD-JWT VC PoH validation for the external-wallet path (future; the
  agent-signed path's spend authority lives in the identity issuer, see
  [ADR-0008](../adrs/0008-compliance-authority-placement.md)).

### Out of scope

- ZIP-32 (HD derivation): zally owns this.
- ZIP-304 (Sapling address signatures): not used by zpay's surface.
- ZIP-310 (viewing key properties): zentity owns this.
- ZIP-312 (FROST for Zcash): not used by zpay's facilitator wallet.
- ZIP-325 (account metadata keys): zentity owns this.
- v4 transactions: deliberately not supported.

## Cross-references

- [Operational surfaces](operational-surfaces.md): readiness probe, ops
  listener, metrics, env-var schema.
- [Facilitator plane](facilitator-plane.md): prepare, settle, confirm, verify
  lifecycle and typed errors at each boundary.
- [Upstream platform binding](upstream-platform-binding.md): what zpay
  expects from zally, zinder, and zentity.
- [Error vocabulary](../reference/error-vocabulary.md): every typed error
  with retry posture.
- [ADR index](../README.md): locked architectural decisions.
