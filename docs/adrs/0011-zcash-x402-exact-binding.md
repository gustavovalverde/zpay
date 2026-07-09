# 0011: Zcash x402 exact binding

## Status

Accepted.

## Context

x402 v2 defines a transport-neutral envelope and an HTTP facilitator API. The
`exact` scheme is implemented per network: each network binding must define the
network identifier, asset identifier, payment authorization material, verify
rules, and settlement rules.

The upstream x402 repository does not define a Zcash `exact` binding today.
zpay still needs a stable contract so resource-server authors, wallet authors,
and agents can implement against one vocabulary without copying the older zpay
lifecycle routes into the x402 namespace.

The Zcash pieces already exist in ZIPs:

- ZIP-321 defines payment request URIs, ZEC amount syntax, and memo transport.
- ZIP-316 defines Unified Addresses and network-specific address prefixes.
- ZIP-302 defines memo byte conventions.
- ZIP-311 defines payment disclosures for post-settlement receipt proof.
- ZIP-374 defines PCZT, the Partially Created Zcash Transaction format.

## Decision

zpay defines and implements a Zcash `exact` binding named
`x402-zcash-exact-v1`.

The x402 `PaymentRequirements` fields are:

| Field | Binding rule |
|-------|--------------|
| `scheme` | `exact` |
| `network` | `zcash:mainnet`, `zcash:testnet`, or `zcash:regtest` |
| `amount` | Integer zatoshis as a decimal string, greater than zero and no more than 21,000,000 ZEC |
| `asset` | `ZEC` |
| `payTo` | ZIP-316 Unified Address whose prefix matches `network` |
| `maxTimeoutSeconds` | Maximum wall-clock window accepted by the resource server |
| `extra.binding` | `x402-zcash-exact-v1` |
| `extra.amountUnit` | `zat` |
| `extra.zpayPaymentId` | Optional zpay-owned lifecycle id when the requirements came from `/zpay/v1/prepare` |

The x402 `PaymentPayload.payload` object for this binding is:

```json
{
  "format": "pczt-v2-extractable",
  "pczt": "<base64url ZIP-374 PCZT bytes>"
}
```

`pczt-v2-extractable` means a ZIP-374 PCZT that can be extracted into a
broadcastable Zcash transaction after zpay verifies the transaction effects.
The facilitator must reject authorization material that is not PCZT. Raw
transaction hex is not a valid Zcash x402 exact authorization because it does
not let the facilitator prove shielded recipient and amount semantics before
broadcast.

Verification for this binding proves all of these before returning
`isValid: true`:

- `x402Version` is 2 and the selected `accepted` requirements equal the
  supplied `paymentRequirements`.
- The PCZT is a ZIP-374 PCZT that parses and can be extracted.
- The payment amount equals `PaymentRequirements.amount` exactly.
- The recipient equals `PaymentRequirements.payTo`.
- The selected x402 network matches the configured chain plane used for
  settlement.
- When `extra.zpayPaymentId` is present, it names an existing prepared row
  whose network, recipient, amount, and expiry match the PCZT-backed
  requirements before zpay broadcasts.

Current verifier scope:

- Sapling, Orchard, and Ironwood labelled outputs are verified by comparing
  PCZT recipient bytes against the requested Zcash address.
- Transparent labelled outputs fail closed until zpay can verify
  `script_pubkey` against `payTo`.
- PCZT `global.coin_type` is not exposed by the current upstream `pczt` API,
  so zpay enforces network through the x402 network id, address parser, and
  chain-plane network check. A future zally or librustzcash API should expose
  that field so zpay can add a pre-extraction coin-type check.
- `maxTimeoutSeconds` is carried and validated structurally. The current code
  does not yet map that wall-clock window to a chain-tip expiry window.
- Replay protection is provided by the signed PCZT transaction id and the
  resource requirement equality check. A future binding revision may add a
  resource hash inside PCZT proprietary fields.

Settlement must extract the PCZT, broadcast the transaction through the chain
plane, and return the txid in the official x402 `SettlementResponse`. When the
optional `extra.zpayPaymentId` extension is present, zpay also records the
settlement in the lifecycle ledger and removes the prepared row on a
success-kind broadcast outcome.

## Current implementation posture

The current code advertises the configured chain network from
`/x402/v2/supported`, verifies signed PCZT payment effects at
`/x402/v2/verify`, and extracts plus broadcasts the same PCZT at
`/x402/v2/settle`. Because extraction uses `zally-pczt::Extractor`, the
facilitator runtime must have Sapling verifying parameters available through
the platform default ZcashParams directory.

## Consequences

Agents get stable reason strings when they send an invalid Zcash exact request
to the facilitator:

- `zcash_exact_asset_unsupported`
- `zcash_exact_amount_invalid`
- `zcash_exact_pay_to_invalid`
- `zcash_exact_authorization_malformed`
- `zcash_exact_authorization_format_unsupported`
- `zcash_exact_pczt_malformed`
- `zcash_exact_network_mismatch`
- `zcash_exact_pay_to_mismatch`
- `zcash_exact_amount_mismatch`
- `zcash_exact_pczt_not_verifiable`
- `zcash_exact_pczt_not_extractable`
- `zpay_payment_id_invalid`
- `zpay_payment_not_prepared`
- `zpay_payment_requirements_mismatch`
- `zpay_payment_expiry_mismatch`
- `chain_unavailable`

The network identifiers are CAIP-2-style identifiers. If the upstream x402 or
CAIP ecosystem accepts different Zcash identifiers, this ADR must be
superseded before zpay changes discovery.

The `/zpay/v1/*` DPoP lifecycle remains a product API and does not become the
x402 binding.
