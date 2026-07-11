# zpay Product Requirements

| Field | Value |
| ----- | ----- |
| Status | Draft |
| Date | 2026-05-25 |
| Owner | ZFND (Gustavo Valverde) |
| Reference consumer | [zentity](https://app.zentity.xyz) Aether AI shopping agent, plus any third-party x402 client |

## Problem Statement

AI agents that pay for things on the open web have one production-grade rail
today, and it is EVM stablecoins on Base. Zcash has the only shielded
settlement layer with mainnet maturity, but the developer experience for
"send me a ZEC payment for this resource" is hand-rolled per project. There
is no shared facilitator that speaks a standard agent-payment wire protocol
(x402, MPP), validates compliance, broadcasts through a maintained indexer,
and confirms settlement back to the agent. As a result, every Zcash-curious
merchant and every Zcash-aware agent has to build the same plumbing twice,
poorly, and the friction is enough that nobody does.

zpay closes that gap. It is the payments-protocol layer that turns the
existing Zcash vertical stack (zebrad -> zinder -> zally) into a callable
HTTP service that agents can pay against with one header swap.

## Source References

- [x402 v2 spec](https://docs.x402.org): HTTP 402 micropayments, the wire
  shape zpay accepts on `/x402/v2/*`.
- [Machine Payments Protocol (MPP)](https://mpp.dev): the second wire shape
  zpay mounts behind a feature flag at `/mpp/v1/*`.
- [ZIP-321](https://zips.z.cash/zip-0321): Zcash payment request URIs. The
  facilitator advertises and parses these.
- [ZIP-302](https://zips.z.cash/zip-0302): 512-byte memo field. zpay's
  protocol memo budget is computed against this limit.
- [ZIP-317](https://zips.z.cash/zip-0317): conventional fees. zpay defers
  fee selection to zally.
- [ZIP-311](https://zips.z.cash/zip-0311): payment disclosures. zpay calls
  Zally's experimental verifier in-process and fetches the exact mined
  transaction context from zinder.
- [ZIP-316](https://zips.z.cash/zip-0316): unified addresses. The only
  recipient form zpay's `accepts[]` advertises.
- [ZIP-225](https://zips.z.cash/zip-0225): v5 transaction format. Required
  for Orchard outputs (the only output kind zpay produces).
- [ZIP-244](https://zips.z.cash/zip-0244): transaction identifier. zpay
  surfaces v5 `txid` as the settlement primary key.
- RFC-0048 (zentity): Zcash x402 agent payments. The cross-stack design
  document zpay implements.
- PRD-42 (zentity): cross-stack integration plan covering zpay's wiring with
  fauzec, zally, zinder, zexplorer, and zentity.

## Product Positioning

zpay is a **Zcash-native payments facilitator**, not a wallet, not an indexer,
not an identity issuer. It owns:

- Wire protocol translation (x402 v2, MPP) into a single internal payment
  lifecycle.
- A short-TTL prepared-transaction cache and idempotency ledger.
- A broadcast and confirmation oracle that talks to zinder.
- Validation of zentity-issued Proof-of-Human SD-JWT-VC tokens for the
  optional compliance extension.

zpay does **not** own:

- Spending keys for any user-initiated payment. The user's wallet signs the
  unbroadcast transaction; zpay only holds it for the time it takes the
  merchant to validate the PoH token, then broadcasts.
- Chain state. Every block-, mempool-, or tree-state read goes through
  zinder.
- Wallet construction. Every transaction-shaping operation goes through
  zally (`Wallet::propose`, `Wallet::send_payment`, the PCZT round-trip).
- Identity. zentity issues PoH tokens; zpay only validates them.
- A user interface. zpay is callable, not browseable.

Audiences, in priority order:

1. **Agents**: machine clients calling x402 or MPP endpoints. Optimised for.
2. **Wallet developers and merchants**: humans wiring zpay into a product.
3. **Operators**: humans running zpay at scale.
4. **Contributors**: humans extending zpay's wire surface.

## ZIP-Driven and Spec-Driven Product Considerations

**x402 v2** forces zpay's standards boundary to be HTTP-native and explicit
about what it supports. The official facilitator surface is only
`/supported`, `/verify`, and `/settle`; `PAYMENT-REQUIRED`,
`PAYMENT-SIGNATURE`, and `PAYMENT-RESPONSE` headers are the contract. zpay
must not advertise a Zcash x402 payment kind until
`x402-zcash-exact-v1` PCZT verification and settlement can prove network,
asset, recipient, amount, resource, timeout, and replay semantics. zpay's
prepare and status lifecycle remains product-owned under `/zpay/v1/*`.

**MPP draft** is in motion and may reshape its surface during zpay's
implementation. Phase 5 keeps `zpay-mpp` feature-gated off by default so the
production deploy can ignore MPP until the spec is stable enough to support.

**ZIP-321** sets the recipient-URI shape. Agents that want to render a
"pay this" deep-link receive a `zcash:` URI from zpay; agents that pay
programmatically attach the prepared `payment_id` instead.

**ZIP-302** caps the memo at 512 bytes. zpay's protocol memo content is
98 bytes (protocol byte, version, challenge hash, resource hash,
evidence_pack_hash), leaving 414 bytes for future extensions. Every byte
above the 98-byte budget needs an ADR.

**ZIP-317 fees** are computed by zally. zpay never sets a fee directly; it
asks `Wallet::propose` for a proposal, which calls the ZIP-317 fee policy.

**ZIP-311 disclosures** are the proof-of-receipt primitive for shielded
payments. zpay verifies Zally-produced Draft1 Sapling and Zally Ironwood
disclosures in-process. Zinder supplies the exact mined transaction bytes and
height used as verification context.

**ZIP-225 + ZIP-244** lock zpay to v5 transactions and v5 txids. v4 is not
supported on any surface.

**ZIP-316 unified addresses** are the only recipient form zpay accepts on
`pay_to`. Sapling-only and transparent-only addresses are rejected with
typed errors.

## Product Verdict

The Zcash stack already does almost everything zpay needs. zebrad validates,
zinder indexes, zally constructs transactions, zentity issues identity
tokens. What is missing is the **payments protocol layer** that exposes
these to the open web behind a standard wire shape. zpay is that layer and
nothing more.

What is easy on the current stack:

- Transaction construction (zally).
- Broadcast (zinder).
- Confirmation tracking (zinder ChainEvents, zexplorer per-txid watch).
- Compliance binding (zentity PoH tokens).

What is hard, and therefore zpay's job:

- Speaking a standard agent-payment wire protocol (x402, MPP) over HTTPS.
- Holding a user-signed unbroadcast transaction safely for a few minutes.
- Idempotency: an agent retrying `settle` must not double-broadcast.
- Confirmation oracle that abstracts over zinder direct vs zexplorer
  fallback so agents do not need to know which is reachable.
- Memo construction that binds the on-chain payment to the off-chain
  identity attestation (evidence_pack_hash).

What zpay deliberately keeps separate:

- Mainnet key custody. Phase 1 of zpay is testnet-only. Mainnet operator
  custody is a separate project with HSM-backed `SeedSealing` (see
  [ADR-0002](adrs/0002-zally-embedded-as-library.md)).
- Browser UI for humans paying with ZEC. That belongs in a wallet
  (Zashi, Zodl, Zallet UI), not in zpay.
- A second payment abstraction inside zentity. zentity's MCP `purchase`
  tool calls zpay over HTTPS for `scheme: "zcash"`, same way it calls a
  future Base x402 facilitator for `scheme: "evm"`.

## Architecture Requirements

zpay is a single facilitator binary (`zpay-runtime`) composing seven crate roles:

| Crate | Boundary | Owns |
|---|---|---|
| zpay-core | Library; never starts a runtime. | Domain types, prepare/oracle/broadcast/compliance modules, capability strings. |
| zpay-dpop | Library. | Pure RFC 7638 JWK thumbprint and RFC 9449 `htu` canonicalization used by the DPoP verifiers. |
| zpay-store | Library; libSQL only. | Prepared-tx cache, settlement ledger, bearer-key-hash table, schema migrations. |
| zpay-x402 | Library; Axum router only. | x402 v2 route handlers, DPoP middleware, x402-specific request/response codecs. |
| zpay-mpp | Library; Axum router only; feature-gated. | MPP route handlers. Mounted disabled in Phase 4; enabled in Phase 5. |
| zpay-runtime | Binary. | Composition root, env-driven config, ops listener, tracing, signal handling, OpenAPI generation. |
| zpay-testkit | Library; test-only. | Agent-payment client fixtures, `require_live()` gates, mock chain source, mock submitter, settlement fixtures. |

Network awareness is non-negotiable per the
[public interfaces spine](architecture/public-interfaces.md#network-tagged-everywhere).
Every domain type carries a `Network` value; constructors fail closed on
mismatch.

## Capability Requirements By Surface

### x402 facilitator (R-FAC-*)

#### R-FAC-1. Advertise supported x402 payment kinds

Now: `/x402/v2/supported` returns the official response shape and advertises
the configured Zcash `exact` kind.
Why it belongs in zpay: agents need a standards-owned discovery surface they
can trust without learning zpay's product lifecycle routes.
Implemented behavior: advertise `x402-zcash-exact-v1` only for the configured
chain network.
Capability: `x402.v2.supported`.

#### R-FAC-2. Verify an x402 payment authorization

Now: `/x402/v2/verify` accepts the official
`{ x402Version, paymentPayload, paymentRequirements }` request and returns
`isValid: true` for signed PCZTs whose labelled shielded payment effects match
the Zcash exact requirements, or `isValid: false` with binding-specific
rejection reasons.
Why it belongs in zpay: x402 agents call facilitator verification before
retrying a protected resource request with payment authorization.
Implemented behavior: parse ZIP-374 PCZT bytes, verify recipient and amount,
then extract transaction bytes to prove extractor readiness.
Capability: `x402.v2.verify`.

#### R-FAC-3. Settle an x402 payment authorization

Now: `/x402/v2/settle` accepts the official facilitator request shape and
extracts and broadcasts valid signed PCZTs, returning the transaction id in the
official settlement response. Invalid requests return `success: false` with
binding-specific rejection reasons.
Why it belongs in zpay: x402 resource servers expect one standards-owned
settlement endpoint that returns the authorization result and settlement
evidence.
Implemented behavior: reuse the same PCZT verification path as `/verify`,
submit extracted transaction bytes through the chain plane, and return the
extracted txid.
Capability: `x402.v2.settle`.

#### R-FAC-4. Run zpay's Zcash payment lifecycle

Now: `/zpay/v1/*` exposes `accepts`, `prepare`, `settle`, `verify`,
`payments/{payment_id}`, and `payments/{payment_id}/events`.
Why it belongs in zpay: the product still needs a Zcash-native lifecycle while
the official x402 Zcash PCZT settlement path is incomplete.
Proposed change: keep this lifecycle out of `/x402/v2/*` and treat it as
zpay-owned orchestration for demos, harnesses, and future product APIs.
Capability: `zpay.v1.accepts`, `zpay.v1.prepare`, `zpay.v1.settle`,
`zpay.v1.verify`, `zpay.v1.payments`.

#### R-FAC-5. Verify a payment disclosure

Now: `/zpay/v1/verify` verifies ZIP-311 Draft1 Sapling evidence in-process,
fetches mined transaction context from zinder, and reconciles recipient,
amount, and the merchant's expected disclosure message independently from
proof validity.
Why it belongs in zpay: shielded payments require ZIP-311 disclosure to
prove a specific recipient received a specific amount.
Shipped contract: `POST /zpay/v1/verify` accepts
`{ txid, expected_amount_zat, expected_pay_to,
expected_disclosure_message_hex, disclosure_payload_hex }`,
uses Zally's experimental payment-disclosure crate, and returns a typed
five-axis verdict.
Capability: `zpay.v1.verify`.

### MPP facilitator (R-MPP-*)

#### R-MPP-1. MPP wire shape

Now: nothing exists. MPP spec is in motion (May 2026).
Why it belongs in zpay: same protocol-neutral core supports both wires; the
second adapter validates that the core is genuinely protocol-neutral.
Proposed change: a separate `crates/zpay-mpp` mounting MPP routes under
`/mpp/v1/*`. Phase 5. Until then, `zpay-mpp` is a stub returning 501.
Capability: `mpp.v1.accepts`, `mpp.v1.prepare`, `mpp.v1.settle`,
`mpp.v1.payments`.

### Prepared-tx cache (R-CACHE-*)

#### R-CACHE-1. Idempotent prepare

Now: nothing exists.
Why it belongs in zpay: an agent that retries `prepare` must not produce
two distinct `payment_id`s for the same logical payment intent.
Proposed change: cache key is `(merchant_id, resource_hash, agent_dpop_jkt,
nonce)`. Within the TTL window, repeat preparations return the same
`payment_id` and the same memo content.
Capability: `cache.prepare.idempotent`.

#### R-CACHE-2. TTL discipline

Now: nothing exists.
Why it belongs in zpay: prepared transactions become stale (`expiry_height`
overruns) and bytes in the cache leak memory.
Proposed change: default TTL 5 minutes; configurable per merchant up to
30 minutes. Cleanup runs every 60s. Expired entries are reported under
`zpay.v1.payments` as `status: expired`.
Capability: `cache.prepare.ttl`.

#### R-CACHE-3. Settlement ledger

Now: nothing exists.
Why it belongs in zpay: post-broadcast, the facilitator must remember which
`payment_id` mapped to which `txid` so confirmation polling resolves.
Proposed change: a `settlement_ledger` table holding
`(payment_id, txid, broadcast_at_unix_seconds, broadcast_outcome,
last_confirmation_check_at_unix_seconds, current_confirmations,
expected_evidence_pack_hash, watch_id)`. Append-only. No row deletion.
Capability: `cache.settlement.ledger`.

### Broadcast oracle (R-BCAST-*)

#### R-BCAST-1. Single-source broadcast

Now: nothing exists.
Why it belongs in zpay: agents must not need to know which zinder endpoint
is reachable.
Proposed change: zpay-core's `broadcast` module wraps
`zinder_client::RemoteChainIndex::broadcast_transaction`. Typed outcome
maps onto x402's `broadcast_outcome` enum.
Capability: `broadcast.transaction.v1`.

#### R-BCAST-2. Watch-or-poll confirmation

Now: nothing exists.
Why it belongs in zpay: agents that subscribe and agents that poll need
identical typed status.
Proposed change: zpay-core's `oracle` module subscribes to zinder
`ChainEvents` for live processes; agents polling `GET /payments/{id}` read
from the settlement ledger updated by the subscription. When the local
zinder is unreachable, the oracle falls back to zexplorer's
`POST /transactions/{txid}/watch` endpoint (see
[upstream-platform-binding.md](architecture/upstream-platform-binding.md)).
Capability: `broadcast.oracle.confirm_v1`.

### Compliance verification (R-COMPL-*)

Where compliance authority lives, and why zpay runs no Proof-of-Human gate at
`/settle`, is decided in
[ADR-0008](adrs/0008-compliance-authority-placement.md). For the agent-signed
path, spend-policy authority is the identity issuer's (Proposal-0003
D-1/D-2); merchant-side PoH validation is future scope for the external-wallet
path only. The three requirements below resolve to that ADR.

#### R-COMPL-1. PoH token verification

Now: nothing exists.
Why it belongs in zpay: merchants need a typed gate that rejects payments
from agents that fail the merchant's `min_verification_level`.
Proposed change: zpay-core's `compliance` module fetches zentity's JWKS at
`https://app.zentity.xyz/api/auth/oauth2/jwks`, caches per ETag, validates
SD-JWT-VC signatures (EdDSA), enforces audience binding, DPoP thumbprint
match (`cnf.jkt`), expiry, and `verification_level >= min_verification_level`.
Capability: `compliance.poh.verify_v1`.

#### R-COMPL-2. Merchant-pairwise subject

Now: nothing exists.
Why it belongs in zpay: merchants need a stable per-payer identifier for
rate-limiting and dedup without learning user identity.
Proposed change: the PoH token's `merchant_sub` claim is a pairwise
subject derived as `HMAC-SHA256(PAIRWISE_SECRET, user_id + merchant_id)`.
zpay validates the claim's presence and shape; zentity owns derivation.
Capability: `compliance.poh.pairwise_v1`.

#### R-COMPL-3. Evidence-pack binding

Now: nothing exists.
Why it belongs in zpay: the on-chain memo binds the payment to the
off-chain compliance evidence pack so future audits resolve.
Proposed change: zpay-core's `prepare` writes the 32-byte
`evidence_pack_hash` into bytes 67-98 of the ZIP-302 memo (after protocol
byte, version, challenge hash, resource hash). See PRD-42 Decision 11.
Capability: `compliance.evidence.bind_v1`.

## Security and Privacy Requirements

- **No user spending keys at rest in zpay.** The user's wallet signs the
  unbroadcast transaction. zpay holds it for at most a TTL window. See
  [ADR-0002](adrs/0002-zally-embedded-as-library.md).
- **Operator keys sealed at rest.** Any operator-owned wallet inside zpay
  uses zally's age-encrypted `SeedSealing` by default. Plain-text seeds are
  refused unless `unsafe_plaintext_seed` is set and a WARN log fires on
  every open.
- **No PII in zpay.** zentity owns PII; zpay only sees the derived
  SD-JWT-VC claims (`verification_level`, `verified`, `sybil_resistant`,
  `merchant_sub`).
- **No PII in memos.** The 98-byte protocol memo carries only protocol
  bytes, hashes, and the evidence-pack hash. The remaining 414 memo bytes
  are reserved; no field that could leak PII is permitted.
- **Bearer keys hashed at rest.** Allowlist tables store SHA-256 hashes
  with a per-deployment salt; raw keys never persist.
- **Constant-time comparisons everywhere a secret is compared**
  (subtle::ConstantTimeEq or equivalent).
- **Origin pinning.** PoH tokens are audience-bound to a merchant origin;
  zpay rejects tokens whose `aud` does not match the merchant the
  `payment_id` was prepared for.
- **DPoP-bound settle.** `settle` requires the agent's DPoP proof to bind
  to the JKT recorded at `prepare` time.

## Data Freshness Requirements

Every response carries a freshness envelope mirroring zinder's pattern (see
[zinder ADR-0011](https://github.com/gustavovalverde/zinder/blob/main/docs/adrs/0011-explorer-freshness-envelope.md)):

```json
{
  "data": { ... },
  "freshness": {
    "network": "zcash:testnet",
    "tip_height": 3217845,
    "tip_block_time_unix_seconds": 1748212812,
    "derive_lag_blocks": 1,
    "fetched_at_unix_seconds": 1748212814
  },
  "capabilities": ["zpay.v1.prepare", "zpay.v1.settle", "zpay.v1.payments"]
}
```

`derive_lag_blocks` is sourced from zinder's `ChainEpoch`. When the
upstream is stale (`derive_lag_blocks > 16`), `prepare` and `settle` return
HTTP 503 with typed `Reason::ChainStale`.

## API Requirements

- HTTP/1.1 and HTTP/2 over TLS in production. Plain HTTP only on
  `127.0.0.1` for local dev.
- OpenAPI 3.1 spec at `/openapi.json`, generated by utoipa from route
  signatures. The spec is the wire contract.
- Every error response is a typed `Problem` document (RFC 9457) with the
  zpay error vocabulary as `type`.
- Idempotency: `Idempotency-Key` header accepted on `prepare` and
  `settle`; reused keys return cached results without re-executing.
- Rate limiting: per `dpop_jkt`, per `merchant_id`, per IP. Defaults
  configurable; emit Prometheus counters on each axis.
- CORS: locked to a configured allowlist; default is empty (no
  cross-origin access).

## UX Quality Bar

- An agent integrator's first successful payment, against a fresh zpay
  deployment, takes under 15 minutes from reading the README to
  observing a `status: confirmed` response.
- A merchant integrator's first `accepts[]` advertisement is one TOML
  edit and one process restart, with no DB migration.
- The validation gate runs in under 90 seconds on a clean checkout
  with warm cargo cache.
- Every error returned over the wire has a runbook entry in
  [docs/runbooks/](runbooks/) within one PR of the error variant landing.

## User Stories

| As a | I want to | So that |
|---|---|---|
| Agent | call the official `/x402/v2/*` facilitator surface when a supported kind is advertised | I can reuse x402 integration patterns without relying on custom zpay routes. |
| Merchant | advertise ZEC acceptance via my zpay deployment's `accepts[]` | my paywall accepts shielded ZEC without me writing wallet code. |
| Merchant | enforce `min_verification_level: "full"` | only KYC-verified humans (via their agents) pay me. |
| Operator | bring up zpay alongside an existing z3 + zinder stack | I deploy one Rust binary, one libSQL DB, and one ops port. |
| Auditor | look up a Zcash payment on-chain and trace it back to its evidence pack | the `evidence_pack_hash` in the memo resolves to a known zentity proof set. |
| Wallet developer | render a `zcash:` URI from a zpay-prepared payment | my user signs in their existing wallet via a deep link. |

## Implementation Milestones

### M0: Foundation (this PRD)

Scaffold, lint baseline, ADRs 0001-0005, public-interfaces.md,
operational-surfaces.md, six crates with skeletons, CI workflow.
**Exit**: validation gate green on an empty workspace.

### M1: Prepare and broadcast over x402 (Phase 4 of PRD-42)

R-FAC-1, R-FAC-2, R-FAC-3, R-CACHE-1, R-CACHE-3, R-BCAST-1.
**Exit**: a Node.js x402 client successfully prepares, settles, and
observes a confirmed testnet ZEC payment.

### M2: Confirmation oracle and verify (Phase 4 continued)

R-FAC-4, R-FAC-5, R-BCAST-2, R-CACHE-2.
**Exit**: a wallet-produced shielded payment disclosure verifies in-process
against transaction context fetched from zinder, and the confirmation oracle
is proven against the configured chain plane.

### M3: Compliance binding (Phase 6 of PRD-42)

R-COMPL-1, R-COMPL-2, R-COMPL-3.
**Exit**: zentity's Aether AI scenario completes a paid request against a
zpay deployment with `min_verification_level: "full"`.

### M4: MPP wire adapter (Phase 5 of PRD-42)

R-MPP-1.
**Exit**: a Node.js MPP client successfully pays through zpay with zero
changes to `zpay-core`.

### M5: Production posture

Shipped: a Prometheus `/metrics` exposition and a dependency-aware `/readyz`
on the ops listener; in-memory fixed-window rate limiting keyed per DPoP
`jkt` and per client IP; an exact-origin CORS allowlist; and operator
runbooks (Railway deploy, reorg recovery, wallet seed). Remaining: ZIP-311
amount reconciliation, observability dashboards, and the mainnet readiness
review.

## Testing Decisions

- **T0 unit** in every crate.
- **T1 integration** in every crate, against fixtures from `zpay-testkit`.
- **T2 perf** added in M2 for the oracle (concurrent settlement load).
- **T3 live** against z3 + zinder regtest in CI nightly, against testnet
  on `workflow_dispatch`.
- ZIP-321 round-trip tests use the upstream conformance vectors from
  zally.
- Memo construction tests use known evidence-pack hashes from zentity's
  attestation evidence table.

## Acceptance Criteria

- All M0 deliverables green on the validation gate.
- The dual-adapter shape compiles with both x402 and MPP feature flags off
  (no dead code in the binary when an adapter is excluded).
- The cross-stack patches land in fauzec, zally, zinder, zexplorer, and
  zentity per PRD-42, each with their own ADR or proposal.
- The OpenAPI spec at `/openapi.json` round-trips through `openapi-typescript`
  and produces a buildable client.

## Resolved Decisions

See the ADR set:

- [ADR-0001](adrs/0001-workspace-and-crate-boundaries.md): Workspace and
  crate boundaries.
- [ADR-0002](adrs/0002-zally-embedded-as-library.md): Zally embedded as a
  library; zpay never holds user spending keys.
- [ADR-0003](adrs/0003-zinder-as-chain-plane.md): Zinder as the chain plane
  source of truth.
- [ADR-0004](adrs/0004-libsql-prepared-tx-cache.md): libSQL for the
  prepared-tx cache and settlement ledger.
- [ADR-0005](adrs/0005-protocol-neutral-core-with-wire-adapters.md):
  Protocol-neutral core with per-wire adapters.

PRD-42 carries the eleven decisions that govern the cross-stack work; this
PRD inherits them and does not relitigate them here.

## Open Questions

1. **MPP spec maturity** at Phase 5 ship time. If the draft is still
   shifting, defer M4 and ship M1-M3 with `zpay-mpp` mounted but
   feature-disabled.
2. **Rate-limit primitive**. Resolved: an in-memory fixed-window counter
   keyed per DPoP `jkt` and per client IP, with a per-process opportunistic
   sweep. No external store; horizontal scaling limits per process.
3. **OpenAPI tool**: utoipa (chosen) vs aide. Switch criterion: if utoipa
   forces hand-written schemas for nested generics in `zpay-core` types,
   reconsider aide.
4. **Per-merchant `merchant_sub` rotation policy**. Stable until
   `PAIRWISE_SECRET` rotates (current) vs rotate on boundary-policy
   revocation. M3 decision.
5. **Evidence_pack_hash same-vs-derivative**. PRD-42 Open Q9; resolve
   before M3.
6. **Mainnet operator-wallet custody**. Out of scope for v1; document the
   constraints and pre-requisites in a follow-up RFC.

## Cross-project asks

Each ask becomes a proposal in the relevant upstream repo. zpay maintains
the proposals in [docs/proposals/](proposals/) until the upstream accepts.

| Upstream | Ask | Status |
|---|---|---|
| fauzec | `CaptchaMode::Bearer` variant for agent-callable claims | Drafted in [PRD-42 Phase 1](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md) |
| zally | `PaymentRequest::to_uri()` method | Drafted in PRD-42 Phase 2 |
| zinder | Exact mined transaction bytes and height for disclosure verification | Implemented through `RemoteChainIndex::transaction_by_id` |
| zexplorer | `POST /api/v1/{network}/transactions/{txid}/watch` route | Drafted in PRD-42 Phase 2 |
| zentity | MCP `purchase` tool `scheme: "zcash"` branch | Drafted in PRD-42 Phase 6 |
| zally | ZIP-311 Draft1 and Ironwood payment-disclosure production and verification | Implemented in rev `6a8a7a4`; see [Proposal-0006](proposals/0006-zally-zip311-disclosure-production.md) |

## Out of Scope

- Mainnet deployment in v1.
- A shared `zcash-auth`, `zcash-capability`, `zcash-money`, or any other
  cross-project Rust crate. The rule-of-three has not fired; revisit when
  a third consumer appears.
- A second payment abstraction inside zentity.
- Browser UI for human payers. Wallets own that.
- FROST threshold custody for zpay's operator wallet. Tracked in zentity's
  RFC-0014 for the user-side guardian flow; zpay's facilitator wallet is a
  separate design.
- ZIP-231 memo bundles. Tracked as a future capacity unlock for the
  PoH-credential signing algorithm question (Ed25519 vs ML-DSA-65 in
  RFC-0039 / RFC-0021).
- Browser-side Halo 2 proving in Noir (zentity RFC-0039 analysis).
- Other agent payment standards (ERC-8004, agent payment intents). Track
  but do not implement until a concrete consumer asks.
