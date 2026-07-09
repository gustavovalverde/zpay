# Facilitator Plane

The facilitator plane is zpay's Zcash payment lifecycle behind the
`/zpay/v1/*` product surface. It is distinct from the official x402 v2
facilitator surface, which is limited to `/supported`, `/verify`, and
`/settle`. The Zcash x402 `exact` binding is `x402-zcash-exact-v1`; the
official x402 surface advertises that binding and settles
`pczt-v2-extractable` authorizations. Each zpay lifecycle step has typed
inputs and typed outputs; no string-typed envelopes cross the plane.

## Lifecycle

```text
  +-----------+   +-----------+   +-----------+   +-----------+
  | advertise |-->|  prepare  |-->|  settle   |-->|  confirm  |
  +-----------+   +-----------+   +-----------+   +-----------+
        |               |              |                |
        v               v              v                v
  accepts[] from   payment_id +   broadcast        status snapshot
  the payee        ZIP-321 URI    outcome from      from the ledger +
  registry TOML    + protocol     zinder            chain view; reorg
                   memo                             downgrades apply
```

A payment holds no user spending key: the payer's wallet signs the
unbroadcast transaction and zpay broadcasts it. What the facilitator proves
at each step, and what it trusts, is fixed by
[ADR-0006](../adrs/0006-facilitator-trust-boundary.md).

## Advertise

The operator pre-registers each payee in the TOML file at
`ZPAY_PAYEES__CONFIG_PATH`:

```toml
[payees.aether-ai-shop]

[[payees.aether-ai-shop.accepts]]
scheme = "zcash"
network = "testnet"
pay_to = "utest1..."
amount_zat = 50000
max_validity_seconds = 120
```

`GET /zpay/v1/accepts?payee_id=aether-ai-shop` returns the payee's
`accepts[]` entries. Each `AcceptsEntry` carries `scheme`, `network`,
`pay_to`, `amount_zat`, `max_validity_seconds`, an optional
`expiry_delta_blocks`, and `merchant_requires_verify` (a UI-affordance flag,
default false). The registry is read once at startup; there is no hot-reload.

## Prepare

Input `PrepareRequest`: `{ payee_id, network, scheme, resource_uri, nonce,
evidence_pack_hash?, idempotency_key? }`. The agent's DPoP proof rides in the
`DPoP` request header, not the body.

Steps:

1. Resolve the payee's `accepts[]` entry for `(scheme, network)`.
2. Compose the protocol memo (`crate::binding::compose_binding_memo`):
   - byte 0: `PROTOCOL_MEMO_TAG` `0xFF` (ZIP-302 Arbitrary; see
     [ADR-0006](../adrs/0006-facilitator-trust-boundary.md))
   - byte 1: `PROTOCOL_MEMO_VERSION` `0x02`
   - bytes 2..34: `challenge_hash = SHA256("zpay/v1/challenge" || ...)`
   - bytes 34..66: `resource_hash = SHA256("zpay/v1/resource" || ...)`
   - bytes 66..98: the supplied `evidence_pack_hash`, present only when one
     is bound

   The prefix is 66 bytes without an evidence pack and 98 bytes with one; the
   wallet zero-pads it to a 512-byte ZIP-302 Arbitrary memo. Callers never
   pre-hash any input; the server derives the domain-separated hashes.
3. Derive `expiry_height` from the chain tip plus the delta.
4. Generate `payment_id` (ULID) and insert the prepared row with TTL.
5. Return `Preparation { payment_id, payment_uri, memo_bytes, expiry_height,
   amount_zat }`.

The product lifecycle agent hands `payment_uri` and `memo_bytes` to the payer's
wallet, which signs and returns `raw_tx_hex`. The official x402 path instead
uses `PaymentPayload.payload.format: "pczt-v2-extractable"`. A generic x402
request settles statelessly. A zpay-prepared x402 request may carry
`PaymentRequirements.extra.zpayPaymentId`; in that case `/x402/v2/settle`
records the broadcast outcome against the prepared row after proving the row
matches the x402 requirements.

## Settle

Input `SettleRequest`: `{ payment_id, raw_tx_hex }`. The DPoP proof is in the
request header.

Steps:

1. Look up the prepared row; 404 if missing or expired.
2. Verify the DPoP `jkt` equals the `jkt` recorded at prepare; refuse a
   mismatch.
3. Parse `raw_tx_hex` through zally and verify the parsed `expiry_height`
   equals the prepared row's. The parse is transaction-version-agnostic
   across v5 and v6. Recipient, amount, and memo content are not checked
   here; that is `/verify`'s job (ADR-0006).
4. Record the settle attempt in `settlement_ledger`.
5. Broadcast through the `zally::Submitter` bound to zinder.
6. Record the typed outcome. The confirmation poll and the chain-event
   subscription pick the row up from here.
7. Return `SettlementOutcome { payment_id, broadcast_outcome, watch_id }`.
   `watch_id` is present only on success-kind outcomes.

`BroadcastOutcome` mirrors zinder's typed response: `accepted`, `duplicate`,
`invalid_encoding`, `rejected`, `unknown`.

There is no Proof-of-Human step at settle. Spend-policy authority for the
agent-signed path lives in the identity issuer (see
[ADR-0008](../adrs/0008-compliance-authority-placement.md)).

## Confirm

### Pull (`GET /zpay/v1/payments/{payment_id}`)

Projects the prepared row, the ledger row, and the shared chain view into a
`PaymentStatusSnapshot`: `{ payment_id, status, intent_posture,
broadcast_outcome, settled_at_unix_seconds, confirmation_count,
mined_block_height, reorg_count, settled }`.

`status` is one of:

- `awaiting`: prepared, not yet settled.
- `broadcast`: success-kind ledger row, not yet observed mined.
- `mined`: observed in a block, below the finality threshold.
- `final`: `confirmation_count >= ZPAY_FINALITY_DEPTH`. A UX milestone, not
  immutability; a reorg still returns it to `broadcast`.
- `failed`: failure-kind broadcast outcome.
- `never_issued`: no prepared and no ledger row.
- `expired`: prepared TTL passed without settling, or an unmined row whose
  `expiry_height` the visible tip has passed.

`settled` is true once the payment's `mined_block_height` is at or below the
chain's settled tip: the point past which no reorg can move it. When a reorg
drops a mined block, the ledger row downgrades (`mined_block_height` clears,
`confirmation_count` zeroes, `reorg_count` increments, `last_reorged_at`
stamps), and the status returns to `broadcast` or `expired`. See
[ADR-0009](../adrs/0009-settlement-lifecycle-and-finality.md).

### Stream (`GET /zpay/v1/payments/{payment_id}/events`)

Server-Sent Events. The stream emits a snapshot on connect and on every
change the background tasks publish (a confirmation, a reorg downgrade, an
expiry lapse). It closes after the first snapshot for which `settled` is true
or the status is terminal (`failed`, `never_issued`, `expired`); a `final`
snapshot that is not yet settled keeps the stream open.

## Verify

Input `VerifyRequest`: `{ txid, expected_amount_zat, disclosure_payload_hex }`.

zpay runs a local ZIP-311 verifier (`zpay-core`; see
[ADR-0007](../adrs/0007-local-zip311-verifier.md)), fed by a
`DisclosureFetcher` backed by zinder's explorer plane (`ZPAY_EXPLORER_URL`).

Output `VerifyResponse` carries three independent axes plus reserved
reconciliation fields:

- `cryptographic_verdict`: `valid`, `invalid_signature`, `malformed`, or
  `inconclusive`.
- `inconclusive_reason` (present only when inconclusive): `unsupported_pool`,
  `unknown_version`, or `prevout_unresolved`.
- `chain_presence`: `mined`, `not_found`, or `oracle_unavailable`.
- `amount_reconciliation`: `match`, `mismatch`, or `not_checked` (every
  response is `not_checked` today; reconciliation lands with a follow-on
  slice).
- `transaction_id`, and the reserved `payment_id` and `disclosed_value_zat`.

A caller reads success as `cryptographic_verdict == valid` and
`chain_presence == mined`.

## Typed errors

Each `zpay-core` module returns its own typed error enum.

`SettleError`:

| Variant | Retry |
|---------|-------|
| `PreparationNotFound { payment_id }` | not_retryable |
| `RawTxHexInvalid` | not_retryable |
| `TransactionMalformed { reason }` | not_retryable |
| `ExpiryHeightMismatch { prepared_expiry_height, signed_expiry_height }` | not_retryable |
| `ObsoleteMemoVersion { observed }` | not_retryable |
| `DpopMismatch` | not_retryable |
| `ChainUnavailable { reason }` | retryable |
| `Storage(StoreError)` | inherits |

`PrepareError`: `PayeeUnknown`, `SchemeNetworkUnsupported`,
`ExpiryHeightInvalid`, `TipOracle`, `Storage`. `VerifyError`: `PayloadInvalid`
(the in-band verdicts flow through `VerifyResponse`, not this enum).
`OracleError`: `Unavailable`, `ResponseMalformed`.

At the wire boundary each error renders as an `application/problem+json`
document with the fields `title`, `kind`, `detail`, and `retryable`. HTTP
status codes are in [error-vocabulary.md](../reference/error-vocabulary.md).

## Idempotency

`prepare` accepts an idempotency key. A second `prepare` from the same DPoP
`jkt` with the same `idempotency_key` returns the original preparation; the
store enforces uniqueness on the composite `(agent_dpop_jkt,
idempotency_key)`. A `duplicate` broadcast outcome at settle is the chain
plane's idempotent success on a re-broadcast of the same transaction.
