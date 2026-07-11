# ADR-0003: Zinder as the chain plane source of truth

| Field | Value |
| ----- | ----- |
| Status | Accepted; disclosure verification amended by [ADR-0015](0015-zip311-draft1-verification.md) |
| Product | zpay |
| Domain | Chain plane, broadcast and confirmation oracle |
| Related | [ADR-0001](0001-workspace-and-crate-boundaries.md), [Upstream platform binding](../architecture/upstream-platform-binding.md), [PRD-42 Decision 4](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md) |

## Context

zpay needs three chain-plane primitives:

- Broadcast a raw transaction and receive a typed outcome.
- Subscribe to confirmation events for a known txid.
- Fetch exact mined transaction context for payment-disclosure verification.

Candidates:

1. **zebrad JSON-RPC direct.** Lowest abstraction; no indexer dependency.
   Missing typed streams, no payment-disclosure verifier, no built-in
   capability advertisement.
2. **lightwalletd.** Mature ecosystem fit but slated for deprecation in
   the modern Zcash stack; protocol is shielded-scan-shaped, not
   broadcast-and-confirm shaped.
3. **zaino.** Successor lightwalletd surface; immature.
4. **zinder.** Next-generation Zcash indexer. Direct Zebra client (no
   lightwalletd dependency). Ships `BroadcastTransaction` typed RPC,
   `ChainEvents` and `MempoolEvents` server-streams with cursor-resumable
   replay, exact transaction reads, and typed capability strings.

zinder is the most aligned with zpay's needs because zinder's typed
`BroadcastTransactionResponse` (`accepted | duplicate | invalid_encoding |
rejected | unknown`) maps cleanly onto x402's `broadcast_outcome` enum,
and `ChainEvents` exposes confirmation tracking as a first-class typed
stream rather than a poll loop.

zexplorer wraps zinder for the chain-read public surface, but zexplorer
is a TypeScript BFF: an unnecessary intermediate hop for a Rust-native
zpay. The exception is when a zpay deployment runs without a directly
reachable zinder (e.g., the operator deploys zpay against a managed
zexplorer); in that case zexplorer's REST + WebSocket surface is the
fallback.

## Decision

**zpay calls zinder directly for the broadcast path and the
confirmation oracle. zexplorer is the fallback when zinder is not
directly reachable.**

- `zpay-core::broadcast` wraps `zinder_client::RemoteChainIndex::broadcast_transaction`.
  The typed `BroadcastTransactionResponse` maps onto zpay's
  `SettlementOutcome::broadcast_outcome` enum without lossy translation.
- `zpay-core::oracle` subscribes to `ChainEvents` for live processes,
  filtered by the addresses zpay has prepared transactions for.
- For the per-txid confirmation lifecycle on a polling agent, the oracle
  reads from zpay's settlement ledger (updated by the subscription) and
  returns the typed `ConfirmationStatus`.
- Fallback: when `ZPAY_CHAIN_SOURCE_URL` is unset or unreachable,
  the oracle calls zexplorer's `POST /api/v1/{network}/transactions/{txid}/watch`
  (delivered to zpay's own `/x402/v2/internal/watch-callback` endpoint).
- Payment-disclosure verification runs in-process using Zally's experimental
  verifier. Zinder supplies the exact mined transaction bytes and height
  through `RemoteChainIndex::transaction_by_id`.

## Rationale

zinder is a deliberate product: typed RPCs, capability strings,
streaming-first design, no `ServiceServer` suffix abuse. Wrapping its
client at the zpay-core boundary keeps zpay's chain plane as deep as
zinder's chain plane.

The zexplorer fallback exists because some zpay deployments will not have
co-located zinder access (e.g., a managed deployment that uses the public
zexplorer instance). The fallback is a one-time HTTP webhook registration,
not an ongoing polling burden.

## Consequences

Positive:

- One source of truth for chain state means zpay's `freshness` envelope is
  always accurate (`derive_lag_blocks` from zinder's `ChainEpoch`).
- The typed `BroadcastTransactionResponse` flows through zpay-core
  without translation, so the agent's `broadcast_outcome` is identical to
  zinder's.
- Confirmation tracking is event-driven (subscription) rather than
  polling, reducing load on both zinder and zpay.

Negative:

- zpay's reliability ceiling is set by zinder's. A zinder outage stalls
  every settlement; the zexplorer fallback partially mitigates by routing
  to a different deployment.
- The pinned `zinder-client` git rev must be bumped in lockstep with
  zinder's wire-protocol changes (rare, but expensive when it happens).

Neutral:

- zexplorer's per-txid watch endpoint is a new upstream ask (PRD-42 Phase
  2). Until it lands, the fallback path is unavailable; v1 ships without
  it and adds it in M2.

## Switch Criteria

Replace this decision when **any** of:

- zinder's wire protocol becomes unstable enough that the pinned `rev`
  needs more than monthly bumps (suggests we should depend on a stable
  released version, not a git rev).
- A second indexer with equivalent typed capability discipline appears
  in the Zcash stack (suggesting the dependency should be polymorphic).
- A zpay deployment shape emerges (e.g., serverless) where direct gRPC
  to zinder is operationally infeasible.

## Alternatives Considered

### zexplorer-first (zinder fallback)

Rejected. Zexplorer is a BFF over zinder; calling it from a Rust process
is a JSON-over-HTTP indirection that loses zinder's typed proto shape.
The right shape is "Rust calls Rust gRPC; TypeScript fallback when the
process cannot reach the gRPC".

### Direct zebrad

Rejected. zebrad's JSON-RPC is the lower abstraction zinder already
wraps with typed RPCs and capability discipline. Reimplementing that
inside zpay duplicates work and loses the capability-string contract.

### Bundle a ZIP-311 verifier in zpay

Rejected. The verifier belongs in zinder (the chain-state owner), not in
a payments-protocol facilitator. Multiple Zcash ecosystem consumers
(zexplorer, zpay, future wallets) verify disclosures; bundling the
verifier in zpay would force every consumer to dedupe verification logic.

## Out of Scope

- Implementing the ZIP-311 verifier inside zinder. Tracked as an upstream
  ask in [zinder's proposals](https://github.com/gustavovalverde/zinder/tree/main/docs/proposals)
  and as a cross-project patch in PRD-42 Phase 2.
- Building zpay's own chain state cache. zinder owns it; zpay's
  `settlement_ledger` is a derived view, not a duplicate.

## Revision history

### 2026-07-05: Event-driven confirmation ships; zexplorer fallback dropped

The confirmation oracle is event-driven against zinder directly. A
`ChainEvents` subscription (`crates/zpay-runtime/src/chain_events.rs`)
tails live from the last cursor, resumes after a cursor expiry, and runs a
full reconciliation sweep on startup and on every cursor expiry. A
60-second poll loop backstops the subscription; both write the
`settlement_ledger`, and the shared chain view they refresh drives the
reorg-aware status projection (see
[ADR-0009](0009-settlement-lifecycle-and-finality.md)). The endpoint is
`ZPAY_CHAIN_SOURCE_URL`.

zpay reaches zinder directly in every supported deployment, so the
zexplorer fallback confirmation oracle is not a configured option. There is
no `ZPAY_NODE__FALLBACK_EXPLORER_HTTP_ADDR` field, no
`POST /api/v1/{network}/transactions/{txid}/watch` registration, and no
`/x402/v2/internal/watch-callback` endpoint. A deployment without a
reachable zinder has no confirmation path: `/settle` returns 502 and
settlement reconciliation stays disabled until `ZPAY_CHAIN_SOURCE_URL` is
set. The Decision's fallback clause and the zexplorer-fallback entries in
Rationale, Consequences, and Out of Scope are superseded by this entry.
