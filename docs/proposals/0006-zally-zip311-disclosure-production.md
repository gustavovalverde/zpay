# Proposal-0006: zally ZIP-311 payment-disclosure production

| Field | Value |
| ----- | ----- |
| Status | Implemented |
| Consumer | zpay |
| Upstream | zally |
| Pinned at | rev `6a8a7a4a3fafce33b188df5eb5c30ba2c627bd85` |
| Related | [ADR-0007: Local ZIP-311 verifier](../adrs/0007-local-zip311-verifier.md), [facilitator-plane.md](../architecture/facilitator-plane.md#verify), [zally#6](https://github.com/gustavovalverde/zally/issues/6) |

## Context

zpay-core verifies wallet-produced payment disclosures through
`POST /zpay/v1/verify`. Producing those disclosures belongs in Zally because
the wallet owns the spending keys and retained transaction material.

## Resolution

Zally now incubates the experimental `zcash-payment-disclosure` crate and
exposes disclosure production through `zally-wallet`:

```rust
impl Wallet {
    pub async fn export_payment_disclosure(
        &self,
        plan: ExportPaymentDisclosurePlan,
    ) -> Result<PaymentDisclosure, WalletError>;
}
```

The plan binds the network-tagged recipient, transaction id, amount, message,
and explicit profile. ZIP-311 Draft1 covers Sapling evidence. Zally's Ironwood
extension covers the post-NU6.3 Orchard-based transaction format while the ZIP
remains a draft.

## Why this lives in zally, not zpay

`zally-wallet` holds the spending keys and the note and witness data a disclosure proves over. zpay never holds a spending key (ADR-0006) and only ever receives disclosure bytes as an opaque hex string; it cannot construct them itself.

## Compatibility

Additive. Existing wallet proposal, signing, and submission callers are
unchanged.

## Acceptance

- `Wallet::export_payment_disclosure` is public and requires a retained
  finalized PCZT.
- The exported bytes verify through Zpay's five-axis verification contract.
- Zpay's demo wallet retains the disclosure with the settled payment and the
  receipts UI submits it to the payment-scoped verification route.
- Draft1 Sapling and Zally Ironwood profiles have separate codec, production,
  verification, and integration coverage.
