# ADR-0013: Shared DPoP primitives

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | DPoP verification |
| Supersedes | The single-consumer shared-crate premise in [ADR-0001](0001-workspace-and-crate-boundaries.md) for RFC 7638 and RFC 9449 primitives only |
| Related | [ADR-0005](0005-protocol-neutral-core-with-wire-adapters.md), [Public interfaces](../architecture/public-interfaces.md) |

## Context

The facilitator and the wallet are separate production adapters with different
DPoP contracts. The facilitator verifies a proof, owns replay admission, and
binds a DPoP `jkt` to the zpay prepare and settle lifecycle. The wallet also
verifies the proof but additionally binds it to an access-token hash and the
token's `cnf.jkt` claim.

Both adapters independently implement two deterministic RFC rules:

- RFC 7638 EC JWK thumbprint derivation.
- RFC 9449 HTTP URL canonicalization for the `htu` comparison.

ADR-0001 rejected shared crates when each candidate had one consumer. That
condition is no longer true for these two pure primitives: both production
adapters use them and each carries the RFC 7638 vector independently. Leaving
the copies independent lets a security-sensitive canonicalization correction
land in one verifier while the other keeps accepting a divergent proof.

## Decision

Add `zpay-dpop`, a small workspace library that owns only:

- `compute_ec_jwk_thumbprint` for RFC 7638 EC keys.
- `canonicalize_http_url` for RFC 9449 `htu` comparison.
- The shared RFC conformance tests for those functions.

`zpay-dpop` is pure: it has no HTTP listener, JWT parser, replay store,
clock, access-token type, or runtime configuration. Its interface accepts
plain strings and returns a typed canonical-URL error.

The adapters retain their different security contracts:

- `zpay-x402` owns proof-header parsing, ES256 verification, replay ordering,
  `jti` limits, clock skew, host pinning, and HTTP problem rendering.
- `zspend-core` owns proof-header parsing, ES256 verification, `ath`,
  `cnf.jkt`, clock skew, and wallet problem rendering.

The existing public thumbprint functions remain available through their
current adapter crates, preserving source compatibility for callers. The
shared primitive is an implementation detail of those adapters.

## Rationale

This is the smallest shared module that passes the deletion test. Removing it
would recreate the same cryptographic serialization and URL normalization in
two production adapters. Including either verifier in the shared module would
create a wide interface and blur the distinct facilitator and wallet seams,
contrary to ADR-0005.

## Consequences

Positive:

- One RFC 7638 vector and canonicalization test surface protects both adapters.
- A correction to the shared RFC rules has one implementation and one review
  point.
- Replay, access-token, and wire-error policies retain their current locality.

Negative:

- The workspace gains one narrowly scoped library crate.
- Dependency changes to the pure primitive require validation in both DPoP
  adapters.

Neutral:

- This does not create a new wire capability, configuration variable, or
  runtime process.

## Switch Criteria

Revisit this decision if a third protocol needs different key types or a
canonicalization policy beyond HTTP and HTTPS. A wider primitive interface
requires a fresh ADR rather than silently extending this crate.

## Alternatives Considered

### Keep the copies

Rejected. The rule of two has fired for deterministic security primitives, and
independent copies make drift more likely than deliberate divergence.

### Move full verification into zpay-dpop

Rejected. The facilitator replay policy and wallet access-token binding have
different interfaces and invariants. Combining them would make a shallow
module with optional modes instead of a deep primitive module.

### Put the primitives in zpay-core

Rejected. DPoP is an HTTP proof contract, not a protocol-neutral payments
domain type. Moving it into zpay-core would violate ADR-0005.
