# ADR-0008: Compliance authority placement

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Compliance, spend authorization, capability surface |
| Related | [ADR-0006](0006-facilitator-trust-boundary.md), [Proposal-0003](../proposals/0003-agent-wallet-production-architecture.md), [product-requirements.md R-COMPL](../product-requirements.md) |

## Context

A merchant who accepts agent-driven ZEC payments wants a policy gate: only
a payer that cleared the merchant's Proof-of-Human bar should be able to
settle. The question this ADR settles is where that gate runs and who owns
the policy, because the answer differs by payer flow.

Two payer flows exist (Proposal-0003, `payee_policy.payer_flow`):

- **Agent-signed.** An AI agent holds a bounded, user-approved
  authorization and drives the payment end to end. The wallet runtime
  (`zspend-runtime`) signs; zpay broadcasts.
- **External.** A human signs in their own wallet (Zashi, Zallet) from a
  ZIP-321 URI or QR; zpay broadcasts what the chain plane observes.

For the agent-signed flow, the authorization is already a structured spend
grant. Proposal-0003 D-1 makes the OAuth access token the grant: a
`payment_authorization` RAR entry (RFC 9396) carried in an `at+jwt` (RFC
9068), DPoP-bound (RFC 9449), with the actor identity in `act.sub`.
Proposal-0003 D-2 makes the identity issuer the sole policy authority: the
issuer evaluates the user's capability against the live usage ledger inside
the CIBA grant handler, and the ledger write is the check. If policy would
be violated, the issuer refuses the mint and the agent never obtains a
token to present.

zpay's settle path is intent-blind by construction (ADR-0006): it proves
network truth (the bytes parse, the expiry height matches, the broadcast
landed) and trusts the surrounding parties for intent. A Proof-of-Human gate
at `/settle` would require zpay to fetch the issuer JWKS, validate an
SD-JWT-VC, and enforce a verification level. That duplicates the policy the
issuer already owns for the agent path, and it has no counterpart at all for
the external path, where no PoH token exists.

The scaffold's capability registry reserved three compliance capability
strings (`compliance.poh.verify_v1`, `compliance.poh.pairwise_v1`,
`compliance.evidence.bind_v1`). No wire surface advertised them and no code
read them: `SettleRequest` carries only `payment_id` and `raw_tx_hex`, and
no JWKS client, SD-JWT-VC verifier, or verification-level check exists
anywhere in zpay.

## Decision

**For the agent-signed path, spend-policy authority lives in the identity
issuer. zpay performs no Proof-of-Human gate at `/settle`. Merchant-side PoH
validation is future scope, scoped to the external-wallet path only.**

- The issuer evaluates `payment_authorization:sign` capability against the
  usage ledger at CIBA mint time (Proposal-0003 D-2). A policy failure
  surfaces as `capability_exhausted` from CIBA, before any token exists.
- The wallet runtime verifies the token's cryptographic envelope (DPoP,
  signature, audience URN, intent hash, recipient match, amount cap,
  single-use `jti`) and refuses to sign anything the RAR does not authorize.
- zpay broadcasts a settle request without inspecting a PoH token. `/settle`
  carries no `poh_token` field and runs no JWKS fetch or SD-JWT-VC
  validation.
- The capability registry advertises no compliance capability. The three
  reserved PoH and evidence-pack capability strings are removed from
  `crates/zpay-core/src/capability.rs`; nothing referenced them.
- Merchant-side PoH validation for the external-wallet path (where the payer
  is a human in their own wallet, not an agent holding a RAR grant) is
  deferred. When it is built, it is a merchant-facing verify surface, not a
  settle-time gate, and it validates an SD-JWT-VC.
- SD-JWT VC is an IETF draft (`draft-ietf-oauth-sd-jwt-vc`). Any validator
  built for the external path pins the exact draft revision it implements,
  so a later draft change is a deliberate version bump rather than silent
  drift.

## Rationale

Distributing policy across three services produces drift: the issuer, the
wallet, and zpay would each hold a partial copy of "may this payer spend
here," they would disagree at the edges, and an integrator would copy
whichever layer they reached first. Proposal-0003 D-2 already names the
issuer as the only place that can atomically read and write usage, which is
what a spend cap requires. A second gate at `/settle` cannot make that
decision more correct; it can only make it inconsistent.

The external path has no PoH token to gate on at settle time, so a settle
gate could never be uniform across payer flows. The clean position matches
ADR-0006: settle is intent-blind for every payer kind, and intent or
compliance confirmation is a separate, merchant-initiated step.

Advertising a capability the code does not implement is a wire lie. An agent
that reads `compliance.poh.verify_v1` from a response would reasonably send a
PoH token to `/settle` and expect enforcement that does not exist. Removing
the strings keeps the capability array honest about what the facilitator
actually proves.

## Consequences

Positive:

- One policy authority for the agent-signed path. Telemetry, revocation, and
  cap accounting live in one place (the issuer plus the wallet usage ledger).
- The capability array matches the code: no advertised compliance capability
  that a caller could rely on and find absent.
- zpay never fetches the issuer JWKS or parses an SD-JWT-VC, so an issuer
  JWKS outage cannot stall settlement.

Negative:

- A merchant on the external-wallet path has no PoH enforcement today.
  Until the merchant-facing validator ships, that flow accepts any signer the
  chain plane confirms.
- The three PoH product requirements (below) have no code behind them in the
  current surface; they are satisfied for the agent path by the issuer and
  the wallet, and deferred for the external path.

Neutral:

- The protocol memo still reserves bytes for an `evidence_pack_hash`
  (ADR-0006). Removing the `compliance.evidence.bind_v1` capability string
  does not remove the memo field; it removes an advertised capability that
  nothing implemented.

## Consequences for PRD R-COMPL requirements

The three R-COMPL requirements in
[product-requirements.md](../product-requirements.md) resolve to this ADR:

- **R-COMPL-1 (PoH token verification).** For the agent-signed path, token
  verification is the wallet runtime's envelope check plus the issuer's
  mint-time capability evaluation, not a zpay `/settle` gate. For the
  external path it is deferred to the future merchant-facing validator.
- **R-COMPL-2 (merchant-pairwise subject).** The pairwise subject is an
  issuer-owned claim (`act.sub` in the agent path). zpay derives nothing.
- **R-COMPL-3 (evidence-pack binding).** The on-chain binding is the memo's
  `evidence_pack_hash` region (ADR-0006), recoverable by a merchant or
  auditor through a ZIP-311 disclosure. There is no separate settle-time
  compliance capability.

## Switch Criteria

Revisit this decision when **any** of:

- A merchant on the external-wallet path needs enforced PoH before the
  facilitator will broadcast, and a settle-time gate (rather than a
  post-settle verify) is the only shape that satisfies the requirement.
- SD-JWT VC reaches RFC status, at which point the deferred validator pins
  the RFC rather than a draft revision.
- A second payer flow appears whose authorization is neither an issuer RAR
  grant nor an external human signature, breaking the two-flow split.

## Alternatives Considered

### PoH gate at `/settle`

Rejected. Duplicates the issuer's authority for the agent path, has no
token to act on for the external path, and adds a JWKS dependency to the
broadcast path. ADR-0006 already commits settle to intent-blindness.

### Keep the compliance capability strings as forward reservations

Rejected. A capability string is a wire promise. Advertising
`compliance.poh.verify_v1` invites callers to send PoH tokens to a surface
that ignores them. Reservations that ship in the wire array are
indistinguishable from implemented capabilities to a caller.

### Merchant-side PoH validation for both payer flows now

Rejected as out of scope. The agent path is already covered by the issuer;
building a second validator for it is redundant. The external path's
validator is deferred until a concrete merchant needs it and the SD-JWT-VC
draft it targets is chosen.

## Out of Scope

- The merchant-facing PoH validator for the external-wallet path. Tracked as
  future scope; it pins an SD-JWT VC draft revision when built.
- The issuer's capability-evaluation internals. Owned by zentity and
  specified in Proposal-0003 D-2.
- The memo's `evidence_pack_hash` layout. Owned by ADR-0006.
