# 0010: x402 public boundary

## Status

Accepted.

## Context

zpay previously mounted its Zcash prepare, settle, verify, status, and event
lifecycle under `/x402/v2/*`. That route shape was useful for the first demo
and local workflow, but it was not the official x402 v2 contract.

Official x402 v2 separates two roles:

- resource servers use `PAYMENT-REQUIRED`, `PAYMENT-SIGNATURE`, and
  `PAYMENT-RESPONSE` headers,
- facilitators expose `GET /supported`, `POST /verify`, and `POST /settle`.

The current Zcash lifecycle also cannot honestly advertise an official x402
`exact` payment kind yet. The official `exact` scheme requires a network
binding that defines the CAIP-2 network id, asset id, authorization material,
recipient proof, amount proof, replay posture, and settlement semantics. zpay
does not have that Zcash binding yet.

## Decision

`/x402/v2/*` is the official x402 facilitator boundary. It mounts only:

- `GET /x402/v2/supported`,
- `POST /x402/v2/verify`,
- `POST /x402/v2/settle`.

Until the Zcash `exact` binding is implemented end to end,
`GET /x402/v2/supported` advertises no supported payment kinds. The official
`verify` and `settle` endpoints return official x402 response shapes with
machine-readable unsupported reasons.

zpay's existing Zcash prepare, settle, status, and event lifecycle moves to
`/zpay/v1/*`:

- `GET /zpay/v1/accepts`,
- `GET /zpay/v1/tip`,
- `POST /zpay/v1/prepare`,
- `POST /zpay/v1/settle`,
- `POST /zpay/v1/verify`,
- `GET /zpay/v1/payments/{payment_id}`,
- `GET /zpay/v1/payments/{payment_id}/events`.

The `/zpay/v1/*` surface is a zpay product API, not an x402 compatibility
surface. The demo and `zpay-e2e` harness may use it while the official Zcash
x402 PCZT settlement path is incomplete.

## Consequences

Integrators get a truthful public contract: official x402 clients will no
longer discover zpay-specific routes under the x402 namespace.

The demo workflow remains runnable because it targets the zpay lifecycle
surface directly.

ADR-0011 defines the Zcash x402 `exact` candidate binding. Future work must
implement PCZT verification and settlement before adding any Zcash kind to
`/x402/v2/supported`. That implementation must prove recipient, amount,
resource, timeout, and replay safety.

The `zpay-x402::wire` module may use exact x402 field names, including
`PaymentPayload` and `payload`, because those names are standards-owned JSON
contract terms. Those names must not leak into protocol-neutral `zpay-core`
types.
