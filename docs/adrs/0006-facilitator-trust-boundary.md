# ADR-0006: Facilitator trust boundary and settle-vs-verify split

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Facilitator wire surface, payment authorization |
| Related | [ADR-0005](0005-protocol-neutral-core-with-wire-adapters.md), [PRD-42 Decision 11](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [Facilitator plane](../architecture/facilitator-plane.md) |

## Context

The first M1 implementation of `/settle` ran a substring check that
expected the prepared 98-byte protocol memo to appear inside
`raw_tx_hex`. The check claimed to catch wallets that dropped the
protocol memo or signed a different transaction than the one zpay
prepared.

The check is incompatible with shielded Zcash transactions. Sapling
and Orchard memos sit inside the AEAD-encrypted output ciphertext
([ZIP-244 §T.3b.ii.1, §T.4b.i][zip244]), keyed by the recipient's
incoming viewing key (`ivk`). A facilitator sitting between the
wallet and the chain plane never sees that key, so the plaintext
memo bytes are indistinguishable from random in the on-chain
encoding. The substring search cannot fire on an honest shielded
send. It would fire only on a transparent send carrying the memo
bytes as transparent script content, a shape Zcash does not have.

The same audit surfaced a second defect: `PROTOCOL_MEMO_TAG`
was `0x5A` ('Z'). ZIP-302 reserves the byte range `0x00..=0xF4`
for UTF-8 text memos whose remaining bytes MUST decode as valid
UTF-8 ([ZIP-302 lines 43-46][zip302]). The protocol memo's 96
bytes of hash material can never satisfy that constraint. The
only ZIP-302 category that admits arbitrary application-defined
payloads is the `0xFF` Arbitrary category ([ZIP-302 lines 58-61]).

Both defects pointed at the same underlying question: what is the
facilitator authoritative on, and what does it trust the surrounding
parties to enforce?

The patterns of comparable facilitators are coherent:

- **x402 v2** ([Coinbase][x402]): the merchant publishes signed
  `PaymentRequirements` in the `402 Payment Required` response. The
  facilitator's `/verify` checks the payer's pre-signed
  authorization matches those requirements; `/settle` broadcasts
  through the chain plane. The merchant is the trust anchor for
  "right recipient, right amount, right asset."
- **Lightning BOLT12** ([lightning/bolts/blob/master/12-...][bolt12]):
  the recipient mints a signed `invoice` with a `payment_hash`. The
  payer presents `{invoice, preimage}` post-payment. No facilitator
  validates intent mid-flow; the recipient-signed invoice is the
  trust anchor.
- **Stripe** ([Webhooks][stripe-webhooks]): the merchant creates a
  `PaymentIntent` on its own server. The authoritative confirmation
  is the HMAC-signed `payment_intent.succeeded` webhook, not the
  client callback. The merchant trusts Stripe's signature, not the
  browser.

In all three the facilitator's authoritative knowledge is narrow:
network truth (broadcast inclusion, signature recovery). Intent
semantics (recipient, amount, memo) come from a recipient-signed
artifact the facilitator does not regenerate.

## Decision

**Settle is a relay with a well-formedness gate. Verify is the
cryptographic gate. The protocol memo carries an Arbitrary tag.**

The split:

| Surface | What it authoritatively checks | What it trusts |
| ------- | ------------------------------ | -------------- |
| `/prepare` | Inputs parse, recipient address is non-empty, amount is non-zero, expiry height is non-zero | Merchant configuration in the accepts registry |
| `/settle` | `raw_tx_hex` parses as a Zcash v5 transaction; its `expiry_height` equals the prepared row's | Wallet built the transaction from the URI zpay returned; recipient and amount binding is the wallet's job |
| `/verify` | ZIP-311 disclosure round-trips through the chain plane's verifier; returns the verdict and the disclosed public facts (`txid`, `payment_id`, `disclosed_value_zat`) | Sender constructed the disclosure with their spending key; caller pairs the verdict with their own knowledge of "is this txid one of my outputs" |

The protocol memo layout stays 98 bytes (`tag | version |
challenge_hash | resource_hash | evidence_pack_hash`); the tag is
now `0xFF`. On chain those 98 bytes sit at the leading region of a
512-byte ZIP-302 Arbitrary memo, zero-padded by the wallet to fill
the remaining 414 bytes. The three hashes act as a session token
that lets a merchant or auditor link a mined tx to its prepared
challenge, resource, and evidence pack statelessly after a
disclosure recovers the memo.

## Rationale

The decision matrix flows from one observation: the facilitator can
prove network truth but cannot prove intent. Network truth covers
"did the bytes parse, did the broadcast land, how deep is it now."
Intent covers "is this the right recipient, right amount, right
memo for the right session." For shielded Zcash, intent lives
behind the recipient's `ivk` and can only be recovered by an actor
who holds it (recipient itself) or who receives a sender-signed
disclosure (anyone with the disclosure bytes plus the tx).

Settle-time intent enforcement requires holding the recipient's
`ivk`, which collapses the privacy property shielded Zcash exists
to preserve. The trade-off is one-way: a merchant gains nothing
material from settle-time content checks that they could not also
get from a post-payment disclosure, but they lose privacy
guarantees to the facilitator the moment they hand over a viewing
key.

`expiry_height` is the strongest property left in the settle path
without those keys. A wallet that signs a tx for a different
expiry than the prepared row was either confused about which
preparation it was settling or compromised. Either way, refusing
to broadcast it is correct.

The `0xFF` tag choice is forced by ZIP-302; there is no other
category that accepts arbitrary bytes.

## Consequences

Positive:

- Wire shape is honest about what the facilitator proves at each
  step. Callers know `/settle` does not vouch for memo content;
  they reach for `/verify` when they need cryptographic certainty.
- The facilitator never needs the merchant's incoming viewing key.
  zpay can be operated by a third party without leaking the
  merchant's transaction visibility.
- The protocol memo is ZIP-302 valid. Every Zcash wallet that
  follows the spec can construct it without bespoke handling.
- The settle path uses `zcash_primitives::transaction::Transaction`
  for parsing instead of ad-hoc byte slicing, which inherits the
  parser's malformed-input handling.

Negative:

- A merchant who needs intent confirmation must run `/verify` after
  the tx mines. That is a second request and a sender-side
  cooperation cost. Both x402 and Stripe accept the same shape via
  webhooks; the alternative is worse.
- The substring check that earlier integration code may have
  relied on for transparent regression tests is gone. Tests of
  transparent receipt go through `/verify` instead.
- The chain plane's ZIP-311 verifier is not yet implemented in
  zinder M0; `/verify` currently returns
  `verdict: capability_unavailable`. A local ZIP-311 verifier in
  zpay-core is a tracked follow-up; until it lands, full
  cryptographic intent confirmation depends on the chain plane.

Neutral:

- The three-hash session-token role of the protocol memo is
  preserved unchanged. ZIP-311 disclosure recovery of the memo
  bytes lets a merchant or auditor stitch a mined tx back to its
  challenge, resource, and evidence pack without zpay-side state.
- The `WatchId` returned from `/settle` and the
  `confirmation_count` on `GET /payments/{id}` are the post-settle
  surface a caller uses when they want network truth without a
  ZIP-311 disclosure.

## Switch Criteria

Replace this decision when all of the following hold:

- A typed wallet attestation scheme exists that lets the wallet
  cryptographically commit to the recipient, amount, and memo it
  signed, without revealing the spending key, AND
- The attestation format is stable across at least two Zcash wallet
  implementations (zally and one of Zashi, Zaino, Zallet), AND
- The facilitator can verify the attestation in single-digit
  milliseconds without holding the recipient's `ivk`.

Until then, the recipient-signed disclosure at `/verify` time is the
strongest cryptographic intent gate available.

## Alternatives Considered

### Substring memo check

Rejected. Documented above: cannot fire on honest shielded sends.

### Hold the merchant's incoming viewing key in zpay

Rejected. Collapses the privacy guarantee that shielded Zcash
provides. A compromised facilitator would disclose every shielded
transaction to that merchant's incoming address. The cost is
permanent; the substring check or recipient verification at settle
time is not worth that cost.

### Use ZIP-302 Future category (`0xF5..=0xFE`) for the protocol tag

Rejected. ZIP-302 lines 51-56 reserve that range for future
specification updates, not for application-defined payloads. The
`0xFF` Arbitrary category is the spec-blessed home for arbitrary
application memos.

### Collapse the three hashes into a single 32-byte session id

Rejected as out of scope for this ADR. The three-hash design is
load-bearing for PRD-42 R-COMPL-3 (evidence-pack binding) and the
challenge/resource separation that future RFC work depends on.
Revisit only if a concrete consumer asks for the collapse.

## Out of Scope

- Implementation of the local ZIP-311 verifier in zpay-core. Tracked
  as a follow-up.
- Wallet attestation format. Out of scope until at least one Zcash
  wallet ships a candidate.
- Recipient-address validation at settle time for transparent
  receivers. Not addressed because the same prepare → settle flow
  is asymmetric across receiver kinds; the cleanest position is
  "settle is intent-blind for every receiver kind."

[zip244]: https://zips.z.cash/zip-0244
[zip302]: https://zips.z.cash/zip-0302
[zip311]: https://zips.z.cash/zip-0311
[x402]: https://docs.x402.org/core-concepts/facilitator
[bolt12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
[stripe-webhooks]: https://docs.stripe.com/webhooks
