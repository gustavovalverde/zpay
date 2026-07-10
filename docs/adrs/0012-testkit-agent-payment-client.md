# ADR-0012: Testkit Owns Agent Payment Client Fixtures

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Dev and test client infrastructure |
| Related | [ADR-0001](0001-workspace-and-crate-boundaries.md), [Public interfaces](../architecture/public-interfaces.md), [x402 public boundary](0010-x402-public-boundary.md) |

## Context

`zpay-demo` and `zpay-e2e` both drive the agent-signed payment flow. Each
binary creates an ephemeral DPoP key, mints a DPoP-bound access token, encodes
the `POST /v1/payments/sign` request, reads its signed-PCZT response, and
constructs the official x402 verify and settle request. The duplicated code
must evolve with the same DPoP, zspend, and x402 contracts.

The binaries deliberately present different error words: the demo maps a
failure into its browser-facing problem shape, while the harness reports an
operator-facing CLI error. A shared client must therefore return typed
technical failures and leave each binary's error translation local.

ADR-0001 defines `zpay-testkit` as a dev-only consumer but lists only
protocol-neutral core and store dependencies. The agent-payment fixtures need
the standards-owned x402 wire types and zspend authorization type, not copied
wire DTOs or a dependency from either binary to the other.

## Decision

`zpay-testkit` owns the dev and test agent-payment client fixtures. Its small
interface provides DPoP key and proof creation, DPoP-bound access-token
minting, the zspend signing request and signed-PCZT response, and construction
of x402 Zcash exact facilitator requests.

The dependency map that supersedes ADR-0001's `zpay-testkit` row is:

```text
zpay-demo --> zpay-testkit
zpay-e2e  --> zpay-testkit
zpay-testkit --> zpay-x402, zspend-core (dev and test consumers)
```

`zpay-testkit` imports `zpay-x402`'s public wire types for the official x402
request and response shape. It does not add a production dependency from
`zpay-runtime`, `zpay-core`, or `zpay-x402` back to the testkit. `zpay-demo`
and `zpay-e2e` remain independent binaries and retain their local error
translations.

## Rationale

Two active consumers make this a real seam. The testkit concentrates one
agent-payment protocol implementation while preserving the locality of each
consumer's user-facing behavior. Reusing the adapter's public x402 wire types
keeps the interface as the test surface and removes locally reconstructed
camel-case JSON shapes.

## Consequences

Positive:

- DPoP, access-token, zspend-sign, and x402 request mechanics change in one
  test-only module.
- Changes to the official x402 wire interface compile through both client
  consumers.
- Browser and CLI error words remain owned by the callers that expose them.

Negative:

- The dev and test binaries compile the testkit and its wire dependencies.

Neutral:

- This decision supersedes only ADR-0001's intended `zpay-testkit` role and
  dependency row. Its protocol-neutral core, store, wire, and runtime boundary
  remains in force.

## Switch Criteria

Replace this decision only when a production caller needs the same client
surface. That caller requires a new ADR that defines its production ownership,
error contract, and dependency direction.

## Alternatives Considered

### Keep duplicate binary-local clients

Rejected. The two copies represent the same protocol contract, so every wire
change has two independent maintenance sites with no added leverage.

### Make zpay-demo and zpay-e2e depend on each other

Rejected. Their UI and CLI roles have independent behavior and error
contracts. A binary-to-binary dependency would invert that ownership.

### Put agent-payment types in zpay-core

Rejected. DPoP, zspend, and x402 client wiring are dev and test concerns, not
protocol-neutral facilitator domain types.

## Out of Scope

- Production HTTP client APIs.
- DPoP verification or replay handling in `zpay-x402`.
- Demo wallet provisioning and e2e wallet orchestration.
