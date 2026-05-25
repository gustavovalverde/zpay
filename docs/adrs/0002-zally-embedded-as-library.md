# ADR-0002: Zally embedded as a library; zpay never holds user spending keys

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Wallet plane, custody model |
| Related | [ADR-0001](0001-workspace-and-crate-boundaries.md), [Public interfaces](../architecture/public-interfaces.md), [Upstream platform binding](../architecture/upstream-platform-binding.md), [PRD-42 Decision 9](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [RFC-0048 (zentity)](https://github.com/gustavovalverde/zentity/blob/main/docs/rfcs/0048-zcash-x402-agent-payments.md) |

## Context

zpay needs Zcash wallet primitives: parse and emit ZIP-321 URIs, validate
unified addresses against a `Network`, compute memo bytes, and (for an
operator-owned hot wallet) propose and sign transactions for facilitator-side
flows (settlement receipt, future faucet-to-merchant rebates).

Three custody candidates:

1. **Broadcaster-only.** zpay never holds any spending keys. The user's
   wallet signs every unbroadcast transaction; zpay only holds the signed
   transaction for the TTL window and broadcasts on settle.
2. **Managed-custody.** zpay holds per-user (or per-agent) hot wallets and
   signs on the user's behalf after CIBA pre-authorization.
3. **Hybrid.** Broadcaster for user-initiated payments; operator-owned
   hot wallet for facilitator-internal flows.

zentity's [Attestation & Privacy Architecture](https://github.com/gustavovalverde/zentity/blob/main/docs/%28architecture%29/attestation-privacy-architecture.md)
states "the server is not trusted for plaintext access". RFC-0048's
specified flow is unambiguous: "user approves -> wallet signs unbroadcast
tx -> facilitator holds -> resource server validates PoH -> resource server
calls settle -> facilitator broadcasts." Managed-custody is the
operationally simplest but the privacy-worst choice; broadcaster-only is
the privacy-best but cannot host operator-side flows that need to sign.

zally is the only Rust wallet library that ships PCZT v0.6 round-trip,
ZIP-321 parser, age-encrypted seed sealing, and a `IdempotencyKey`
primitive. It is also the library fauzec embeds in production today, so
the pattern is proven.

## Decision

**zpay embeds zally as a library, and runs in hybrid custody mode.**

- The user's wallet signs every user-initiated payment. zpay receives the
  signed `raw_tx_hex` on `settle`, validates it (network match,
  recipient match, amount match, expiry-height freshness), and broadcasts
  via zinder. The user's spending key never enters zpay's process.
- An operator-owned hot wallet runs inside `zpay-runtime`, sealed by
  zally's `AgeFileSealing`. This wallet is used only for facilitator-side
  flows: receiving any operator-side fees (future), holding inbound notes
  for facilitator-internal accounting, and exposing a sink for future
  faucet-to-merchant rebates.
- The operator wallet's spending key is sealed at rest. `--print-config`
  redacts every secret. Plain-text seeds require explicit
  `unsafe_plaintext_seed` opt-in and emit a WARN log on every open.
- v1 of zpay runs **testnet only**. Mainnet operator-wallet custody
  requires HSM-backed `SeedSealing` (not yet implemented in zally) and is
  out of scope.

## Rationale

Broadcaster-only is the right shape for **user** payments, and that is the
only flow zpay's external API exposes. The operator wallet is an internal
construct that no external caller touches; it exists to receive payments
addressed to the facilitator (e.g., a faucet rebate inbound), not to sign
on a user's behalf.

Embedding zally as a library (rather than calling a remote zally gRPC) is
the proven pattern from fauzec. zally's `Wallet` is `Clone + Send + Sync`,
sits behind an `Arc`, and survives the entire process lifetime. No gRPC
boundary, no second protocol to version, no remote-procedure-call
performance tax.

## Consequences

Positive:

- Privacy posture matches RFC-0048 verbatim. zpay's operator cannot sign
  user transactions even with full process access; the user's spending
  key is never present.
- Idempotency for operator-side transactions comes free via zally's
  `IdempotencyKey`.
- ZIP-321 parsing reuses zally's existing `PaymentRequest::from_uri`.
- The operator wallet's age-encrypted seed is the same primitive fauzec
  uses; one operational pattern across these Rust services.

Negative:

- The agent's wallet must speak Zcash. Agents that today only call EVM
  x402 endpoints need a Zcash-aware wallet (Zashi, Zodl, Zallet, or a
  zally-embedded browser surface).
- The user must complete an additional step (sign in their wallet) that
  EVM x402 flows avoid by holding the EOA's spending key in MetaMask.
- Mainnet operator-wallet custody is parked behind HSM work in zally.

Neutral:

- The operator wallet is single-account-per-deployment for v1, matching
  zally's current constraint. Multi-account is a zally upstream ask if it
  becomes load-bearing.

## Switch Criteria

Move to managed-custody mode for some flows when **all** of:

- A concrete merchant use case requires zpay to sign on the user's
  behalf (e.g., recurring micropayments below CIBA approval threshold).
- HSM-backed `SeedSealing` lands in zally and is proven against a
  testnet workload.
- zentity's CIBA pre-authorization is extended to cover per-payment
  signing without re-authentication.

## Alternatives Considered

### Managed-custody for all payments

Rejected. Violates RFC-0048's specified flow and zentity's "server not
trusted for plaintext" invariant. zpay would become a custodial wallet
provider, with the regulatory, security, and trust implications that
entails. Not in scope for v1, and not a target shape.

### zpay calls a remote zally over gRPC

Rejected. Adds a gRPC boundary with no isolation benefit (the spending
key would still live in one process, just a different one). Adds latency,
a second protocol surface to version, and an operational dependency. The
proven pattern from fauzec is the in-process library embedding.

### Wallet-less zpay (no operator hot wallet)

Rejected for v1. Future facilitator-side flows (faucet rebates,
operator-side payment receipts, future MPP intermediary roles) need an
operator-owned wallet. Building it from scratch when a future need
appears is more expensive than including it from the start and leaving
it unused.

## Out of Scope

- Mainnet HSM-backed operator-wallet custody.
- Multi-account operator wallets (single account in v1).
- FROST threshold-signed operator custody (tracked in zentity RFC-0014
  for the user-side guardian model; zpay's facilitator wallet is a
  separate design).
