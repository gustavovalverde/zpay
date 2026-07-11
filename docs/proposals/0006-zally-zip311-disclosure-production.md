# Proposal-0006: zally ZIP-311 payment-disclosure production

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Consumer | zpay |
| Upstream | zally |
| Pinned at | rev `3d2b8234e068fb81c4729f54ca4b13b34e763e1f` |
| Related | [ADR-0007: Local ZIP-311 verifier](../adrs/0007-local-zip311-verifier.md), [facilitator-plane.md](../architecture/facilitator-plane.md#verify), [zally#6](https://github.com/gustavovalverde/zally/issues/6) |

## Context

zpay-core runs a local ZIP-311 payment-disclosure verifier (`POST /zpay/v1/verify`, see ADR-0007) that checks `cryptographic_verdict`, `chain_presence`, and `amount_reconciliation` for a disclosure a payer's wallet produces after a spend. `zally-wallet` at the pinned rev has no ZIP-311 disclosure-producing capability: a wallet built on `zally-wallet`, including zpay's own demo wallet (`zpay-demo`), cannot generate the `disclosure_payload_hex` bytes `/verify` expects.

The gap surfaced while building zpay's demo receipts view: the "verify this payment" action calls the real endpoint, but only with an empty disclosure, which always returns a non-fabricated but uninformative `cryptographic_verdict: malformed`. There is no way to demonstrate a real `valid` verdict end-to-end without this capability.

## Ask

Add a disclosure-producing method to `zally-wallet`, mirroring the shape ZIP-311 defines (per-spend witness data, BLAKE2b-digested):

```rust
impl Wallet {
    /// Produce a ZIP-311 payment disclosure for a settled spend.
    ///
    /// Proves the caller controls the spending key for the given transaction's
    /// relevant notes and outputs, without revealing the full viewing key.
    pub async fn disclose_payment(&self, txid: TxId) -> Result<PaymentDisclosure, WalletError>;
}
```

The exact signature and `PaymentDisclosure` wire shape are upstream's call; zpay-core's `disclosure_payload_hex` only needs the serialized bytes.

## Why this lives in zally, not zpay

`zally-wallet` holds the spending keys and the note and witness data a disclosure proves over. zpay never holds a spending key (ADR-0006) and only ever receives disclosure bytes as an opaque hex string; it cannot construct them itself.

## Compatibility

Additive. No existing callers depend on the absence of this method.

## Acceptance

- `Wallet::disclose_payment(txid) -> Result<PaymentDisclosure, WalletError>` exists on the public surface.
- A disclosure produced by this method, hex-encoded, verifies as `cryptographic_verdict: valid` against zpay-core's `LocalPaymentDisclosureVerifier` for a spend that method actually made.
- Documented with a doc link to ZIP-311.

Once accepted: zally bumps its workspace `rev`; zpay bumps the pinned rev, wires `zpay-demo`'s wallet to call `disclose_payment` after a successful settle, and updates the demo receipts view's verify flow to send the real disclosure instead of an empty one.
