# Facilitator Plane

The facilitator plane is the single internal lifecycle that backs every
wire adapter. x402 v2 and MPP differ on the HTTP surface; they share this
plane verbatim.

## Lifecycle

```text
  +-----------+   +-----------+   +-----------+   +-----------+
  | advertise |-->|  prepare  |-->|  settle   |-->|  confirm  |
  +-----------+   +-----------+   +-----------+   +-----------+
        |               |              |                |
        v               v              v                v
  accepts[] in     payment_id +    txid +           confirmations
  config TOML;     ZIP-321 URI    broadcast        from zinder
  merchant         + 98-byte      outcome from      ChainEvents
  registration     ZIP-302 memo   zinder            subscription
```

Each step has typed inputs and typed outputs; no string-typed payloads
cross the plane.

## Advertise

The merchant pre-registers an `accepts[]` template in the operator's
TOML config:

```toml
[merchants.aether-ai-shop]
operator_email = "ops@example.com"
default_validity_seconds = 120
min_verification_level = "full"

[[merchants.aether-ai-shop.accepts]]
scheme = "zcash"
network = "zcash:testnet"
pay_to = "utest1..."
amount_zat = 50000
```

`GET /x402/v2/accepts?merchant=aether-ai-shop&resource=…` returns the
matching template wrapped in zpay's freshness envelope. The TOML is
hot-reloadable on SIGHUP.

## Prepare

Input: `PrepareRequest { merchant_id, resource_uri_b64, agent_assertion,
dpop_proof }`.

Steps:

1. Resolve merchant config from TOML.
2. Compose the protocol memo content (98 bytes):
   - byte 0: protocol byte `0xFF` (ZIP-302 Arbitrary category; see
     [ADR-0006](../adrs/0006-facilitator-trust-boundary.md))
   - byte 1: version `0x01`
   - bytes 2-33: challenge hash (`SHA-256(merchant_id || resource_uri ||
     nonce)`)
   - bytes 34-65: resource hash (`SHA-256(resource_uri)`)
   - bytes 66-97: evidence_pack_hash (derived from agent_assertion)

   The wallet wraps these 98 bytes as a 512-byte `MemoBytes::Arbitrary`,
   zero-padding bytes 98..511.
3. Call zally `Wallet::propose` to build the recipient URI components.
4. Generate `payment_id` (ULID).
5. Insert into `prepared_tx` table with TTL.
6. Return `Preparation { payment_id, payment_uri, memo, expiry_height }`.

Output: `Preparation`. The agent passes `payment_uri` + `memo` to the
user's wallet (deep link, QR, or browser bridge); the user signs and
returns `raw_tx_hex`.

Capability gates the operation behind `x402.v2.prepare` or
`mpp.v1.prepare`.

## Settle

Input: `SettleRequest { payment_id, raw_tx_hex, poh_token, dpop_proof }`.

Steps:

1. Look up `prepared_tx[payment_id]`. 404 if missing or expired.
2. Verify the DPoP proof binds to the JKT recorded at prepare time.
3. Verify the PoH token:
   - Fetch zentity JWKS (cached by ETag, TTL from config).
   - Validate SD-JWT-VC signature (EdDSA).
   - Enforce `aud == merchant_origin`, `cnf.jkt == agent_dpop_jkt`,
     `exp` freshness, `verification_level >= min_verification_level`.
4. Parse `raw_tx_hex` as a Zcash v5 transaction. Verify the parsed
   `expiry_height` equals the `expiry_height` zpay returned at prepare
   time. Recipient, amount, and memo content are NOT verified here;
   that is `/verify`'s job, via a ZIP-311 disclosure (see
   [ADR-0006](../adrs/0006-facilitator-trust-boundary.md)).
5. Insert into `settlement_ledger` with `broadcast_outcome:
   pending_broadcast`.
6. Call `zinder_client::broadcast_transaction(raw_tx_hex)`.
7. Update the ledger row with the typed outcome.
8. Subscribe the confirmation oracle to the txid.
9. Return `SettlementOutcome { txid, broadcast_outcome, watch_id }`.

Output: `SettlementOutcome`. Possible `broadcast_outcome` variants
mirror zinder's typed `BroadcastTransactionResponse`:
`accepted | duplicate | invalid_encoding | rejected | unknown`.

Capability: `x402.v2.settle` or `mpp.v1.settle`.

## Confirm

Two delivery modes:

### Pull mode (`GET /x402/v2/payments/{payment_id}`)

Reads from `settlement_ledger`. The ledger row is updated by the
confirmation oracle's `ChainEvents` subscription. Returns
`ConfirmationStatus { status, confirmations, block_height }`.

Statuses:

- `prepared`: in `prepared_tx`, not yet settled.
- `settled`: in `settlement_ledger`, broadcast succeeded, `confirmations: 0`.
- `confirmed`: `confirmations >= 1`.
- `expired`: prepared_tx expired without settling, or settlement_ledger
  shows `broadcast_outcome != accepted` after retries exhausted.
- `failed`: typed error variant captured in `failure_reason`.

### Push mode (zpay's internal watcher)

The oracle subscribes to zinder `ChainEvents` with an `address_filter`
matching the merchant's `pay_to`. When a new confirmation event arrives,
the oracle updates the ledger row and (optionally) fires a webhook
configured per-merchant. Webhook payload is signed with the merchant's
HMAC secret.

## Verify

Input: `VerifyRequest { txid, expected_amount_zat, expected_pay_to,
disclosure_payload }`.

Steps:

1. Look up the ledger row for the txid (404 if absent).
2. Call `zinder_client::verify_payment_disclosure(disclosure_payload)`.
3. Compare zinder's decoded `(disclosed_amount_zat, disclosed_pay_to)`
   to the `expected_*` inputs.
4. Return `DisclosureVerdict { verdict, disclosed_amount_zat,
   disclosed_pay_to, evidence_pack_hash_match }`.

Verdicts: `valid | mismatch_amount | mismatch_recipient | invalid_signature
| transaction_not_found | capability_unavailable`.

Capability: `x402.v2.verify`.

## Typed errors at each boundary

Each step returns a typed `FacilitatorError` from `zpay-core::error`:

```rust
pub enum FacilitatorError {
    MerchantUnknown { merchant_id: MerchantId },        // not_retryable
    PreparationExpired { payment_id: PaymentId },       // not_retryable
    IdempotencyMismatch { ... },                         // not_retryable
    DpopProofInvalid { reason: DpopRejectionReason },    // not_retryable
    PohTokenInvalid { reason: PohRejectionReason },      // not_retryable
    TransactionMalformed { reason: TxRejectionReason },  // not_retryable
    BroadcastRejected { reason: ZinderBroadcastReason }, // depends on variant
    ChainStale { derive_lag_blocks: u32 },               // retryable
    StoreUnavailable,                                    // requires_operator
    IndexerUnavailable,                                  // requires_operator
    JwksUnavailable,                                     // requires_operator
    CapabilityDisabled { capability: String },           // requires_operator
}
```

`FacilitatorError::into_problem()` maps onto RFC 9457 Problem Details
documents at the wire boundary, with HTTP status codes drawn from
[error-vocabulary.md](../reference/error-vocabulary.md).

## Idempotency

Both prepare and settle accept an `Idempotency-Key` header (RFC 9460
draft). The store records the response body keyed by `(payment_id,
idempotency_key)` or `(merchant_id, agent_dpop_jkt, idempotency_key)`.
Repeated requests within the TTL return the cached response without
re-executing.

The settle path is doubly idempotent: zally's `IdempotencyKey` prevents
double-broadcast at the wallet boundary, and zpay's store-side
idempotency prevents duplicate work above it. Either layer alone would
have edge cases; both together are safe under retries.

## Capability surface evolution

A new capability that adds a step to the lifecycle (e.g., a future
`refund` step) appears as:

1. A new `zpay-core` typed function.
2. A new wire route in each adapter (`POST /x402/v2/refund`, `POST
   /mpp/v1/refund`).
3. A new entry in this document under the appropriate lifecycle section.
4. A new capability string (e.g., `x402.v2.refund`).
5. A new ADR if the step crosses a boundary not yet defined here.
