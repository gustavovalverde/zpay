# ADR-0015: Payment-Disclosure Verification Uses Zally and Zinder

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Payment-disclosure verification |
| Supersedes | ADR-0007 implementation, fetcher, and network-configuration details |
| Related | [ADR-0003](0003-zinder-as-chain-plane.md), [ADR-0007](0007-local-zip311-verifier.md), [ZIP-311](https://zips.z.cash/zip-0311) |

## Context

ADR-0007 established separate cryptographic, chain-presence, and amount
verdicts, but its local parser covered transparent disclosures only. It
deferred Sapling verification, left the zinder fetcher as a stub, and used a
second network variable for a digest format that ZIP-311 Draft1 no longer
uses.

Zpay needs Sapling and Ironwood payment disclosures before ZIP-311 is
finalized. Zally now owns an experimental `zcash-payment-disclosure` crate
with 2 explicit profiles: ZIP-311 Draft1 verifies selected Sapling spends and
recovers authenticated Sapling outputs; the Zally Ironwood extension verifies
message-bound spend signatures against mined action keys and recovers
Ironwood outputs. Zinder's wallet plane returns the exact mined transaction
bytes and height required by both profiles.

## Decision

Zpay delegates ZIP-311 format and cryptographic semantics to
`zcash-payment-disclosure` while the draft matures. The dependency stays
experimental and can move upstream without changing Zpay's product boundary.
Zpay does not maintain a second ZIP-311 parser.

`DisclosureFetcher` returns the exact mined transaction bytes, mined height,
and transaction id. Production uses `RemoteChainIndex::transaction_by_id`
through the existing `ZPAY_CHAIN_SOURCE_URL`. RPC transaction-id bytes are
reversed before constructing zinder's internal `TransactionId`. Zinder must
retain transaction blobs.

`VerifyRequest` carries `txid`, `expected_amount_zat`, `expected_pay_to`,
`expected_disclosure_message_hex`, and `disclosure_payload_hex`.
`expected_pay_to` must match the configured network. Draft1 accepts a bare
Sapling address or a ZIP-316 Unified Address containing the expected Sapling
receiver. The Zally Ironwood profile requires a Unified Address containing
the expected Orchard receiver. The expected disclosure message is required
and remains arbitrary bytes through its hex wire encoding.

`VerifyResponse` keeps the independent cryptographic, chain-presence, and
amount axes from ADR-0007 and adds `recipient_reconciliation` and
`message_reconciliation`. Cryptographic validity reports only proof validity.
Amount, recipient, and expected-message matching are Zpay product policy. A
relying party accepts a receipt only when cryptography is valid, the
transaction is mined, and all reconciliation axes match.

`ZPAY_NETWORK` is the single network authority for verification. Regtest uses
regtest consensus parameters. `ZPAY_VERIFY__NETWORK` and
`ZPAY_EXPLORER_URL` are removed.

Sapling proving parameters must be available at the `zcash_proofs` default
location. Zpay loads the prepared Spend verifying key at startup. Existing
container and compose mounts provide those files.

## Consequences

- Sapling Draft1 and Zally Ironwood disclosures are verified in-process with
  no delegated cryptography RPC.
- The chain plane serves one canonical mined transaction context through the
  same endpoint used for broadcast, tips, and settlement observation.
- A valid proof for the correct amount but another recipient is an explicit
  recipient mismatch, not a cryptographic failure.
- A valid proof carrying another message is an explicit message mismatch, not
  a cryptographic failure.
- Zpay intentionally follows a draft format. Porting the crate upstream may
  change the dependency source, but the Zpay reconciliation contract remains
  stable.
- The Ironwood profile is a Zally extension, not a claim that ZIP-311 defines
  Orchard or Ironwood evidence. Unknown profiles fail closed.
