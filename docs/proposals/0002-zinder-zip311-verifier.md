# Proposal-0002: zinder ZIP-311 payment disclosure verifier

> Superseded by [ADR-0007](../adrs/0007-local-zip311-verifier.md): the local verifier in `zpay-core` replaces the zinder-side approach this proposal described.

| Field | Value |
| ----- | ----- |
| Status | Superseded |
| Consumer | zpay |
| Upstream | zinder |
| Pinned at | branch `main` (no consumer-blocking pin yet) |
| Related | [ADR-0007](../adrs/0007-local-zip311-verifier.md), [PRD-42 Phase 2](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [ADR-0003](../adrs/0003-zinder-as-chain-plane.md) |

## Context

zinder advertises the capability `explorer.payment_disclosure.verify_v1` (`zinder/services/zinder-explorer/src/grpc/adapter.rs:~565`) and exposes the gRPC method `ExplorerQuery::VerifyPaymentDisclosure`, but the actual ZIP-311 verifier is not bundled. The capability is gated off by default and returns `UNAVAILABLE` until an operator opts in; the opt-in currently triggers an unimplemented branch.

zpay's `core::verify::DisclosureVerdict` flow depends on this capability. For shielded payments, ZIP-311 disclosure verification is the only proof-of-receipt primitive that does not require the merchant to hold a viewing key.

## Ask

Land the ZIP-311 verifier inside zinder's explorer service. The verifier implements:

- BIP-340 Schnorr signature verification over secp256k1 for the disclosure signature.
- For Sapling: trial-decryption of the note ciphertext using the disclosed `esk` and the recipient `pk_d` (specified in ZIP-311 section "Disclosure Format").
- For transparent: input/output script verification against the disclosed UTXO.
- Cross-check the disclosed `(amount, recipient)` against the on-chain transaction outputs zinder already indexes.

The flip from `capability advertised, not bundled` to `enabled by default` lands once unit-test coverage hits 90% on the verifier module and at least one live testnet vector verifies green.

## Why this lives in zinder, not zpay

zinder is the chain-state owner. The verifier needs random access to indexed transactions, which is zinder's job; building it into zpay would either force zinder to expose a "give me the raw transaction" RPC (which it already does, via `Transaction`) or push zpay to maintain a chain-state cache (which would duplicate zinder's RocksDB store).

Multiple Zcash ecosystem consumers (zexplorer's `/api/v1/{network}/payment-disclosures/verify` route, zpay's `verify`, future wallets) will verify disclosures. Bundling the verifier in zinder lets every consumer share the same implementation.

## Compatibility

Additive. The capability is already advertised. The flip from "stub" to "implemented" is observable only by callers that already check the capability before calling.

## Acceptance

- ZIP-311 verifier lives at `zinder/services/zinder-explorer/src/payment_disclosure/`.
- `VerifyPaymentDisclosure` returns typed verdicts: `valid | invalid_signature | transaction_not_found | malformed`.
- `explorer.payment_disclosure.verify_v1` is `enabled_by_default: true`.
- At least one T3 live test verifies a real testnet disclosure.

Once accepted: zinder advertises the capability as enabled; zpay's M2 connects `zpay-core::verify::disclosure` to the new RPC.
