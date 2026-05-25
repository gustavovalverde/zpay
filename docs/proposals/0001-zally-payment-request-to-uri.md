# Proposal-0001: zally `PaymentRequest::to_uri()`

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Consumer | zpay |
| Upstream | zally |
| Pinned at | rev `b1123fb435b64d2bb66b3dc1bf48bc4aa236d6ca` |
| Related | [PRD-42 Phase 2](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [Upstream platform binding](../architecture/upstream-platform-binding.md) |

## Context

`zally-wallet` already exposes `PaymentRequest::from_uri(uri, network) -> PaymentRequest` (`zally/crates/zally-wallet/src/spend.rs:41`), the ZIP-321 parser used by every consumer that needs to interpret a `zcash:` URI.

zpay needs the inverse. `zpay-core::prepare` composes a recipient URI for the agent to deliver to the user's wallet (deep link, QR, or browser bridge). Hand-rolling the URI inside zpay duplicates zally's ZIP-321 vocabulary across two crates and creates two places that can drift out of sync with the spec.

## Ask

Add a method to `PaymentRequest`:

```rust
impl PaymentRequest {
    /// Serialise this payment request as a ZIP-321 URI.
    ///
    /// The result round-trips through `PaymentRequest::from_uri` for any
    /// `PaymentRequest` constructed through the public surface. Reserved
    /// parameters surface as the corresponding ZIP-321 query keys.
    pub fn to_uri(&self) -> String;
}
```

Test contract: round-trip every vector in the ZIP-321 conformance suite. Identity property: `PaymentRequest::from_uri(req.to_uri(), network) == Ok(req)` (for any well-formed `req`).

## Why this lives in zally, not zpay

`PaymentRequest` is zally's domain type. The parser already lives there. Putting the generator anywhere else splits ZIP-321 vocabulary across two crates: a future ZIP-321 update would require coordinated changes to two repositories, and consumers would never know which crate to ask about a URI bug.

## Compatibility

Additive. No existing callers depend on the absence of this method.

## Acceptance

- `PaymentRequest::to_uri(&self) -> String` exists on the public surface.
- Round-trip test exists in `crates/zally-wallet/src/spend.rs::tests` covering at least: transparent address, Sapling address, Orchard unified address, payment with memo, payment with amount but no memo.
- The method is documented with a doc link to ZIP-321.

Once accepted: zally bumps its workspace `rev`, then zpay bumps the pinned rev in its `Cargo.toml` workspace dependencies and removes any local URI-construction code introduced before this proposal accepted.
