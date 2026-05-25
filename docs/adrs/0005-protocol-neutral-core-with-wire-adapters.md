# ADR-0005: Protocol-neutral core with per-wire adapters

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Facilitator wire surface |
| Related | [ADR-0001](0001-workspace-and-crate-boundaries.md), [PRD-42 Decision 2](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [Facilitator plane](../architecture/facilitator-plane.md) |

## Context

zpay must support both x402 v2 (the dominant agent-payment standard
today) and MPP (Machine Payments Protocol, positioned in zentity
RFC-0041 as the future primary). The two protocols share the same
underlying payment lifecycle (advertise -> prepare -> settle -> verify)
but disagree on the wire shape, header set, idempotency model, and
error vocabulary.

Two shapes are possible:

1. **One core lifecycle, two wire adapters.** Each adapter speaks its
   protocol's wire shape and delegates to a shared `zpay-core`. The
   core types are protocol-neutral.
2. **Two parallel implementations.** x402 and MPP each carry their own
   core logic. Duplicates the wallet, broadcast, oracle, and compliance
   paths.

The second shape is dismissable on YAGNI grounds: nothing in either
protocol's semantics suggests the underlying Zcash flow differs. Both
prepare a transaction, hold it, broadcast it, and confirm it. The
differences are wire-level.

The harder question is how to enforce the protocol-neutral property of
the core. Possible enforcement mechanisms:

- **Code review only.** Reviewers catch leaks of x402 vocabulary into
  the core. Cheapest but unreliable.
- **Feature gates inside one crate.** Use `#[cfg(feature = "x402")]` to
  isolate adapter code. Compile-checkable but allows the core to import
  from the adapter modules.
- **Separate crates.** `zpay-x402` and `zpay-mpp` depend on `zpay-core`;
  `zpay-core` cannot depend on either. The Cargo dependency graph makes
  the constraint a compile error.

The third option is the strongest enforcement. PRD-42 M4 exit criterion
also requires that "adding `zpay-mpp` does not modify `zpay-core`"; this
is compile-checkable only with the separate-crate model.

## Decision

**Two separate wire adapter crates (`zpay-x402`, `zpay-mpp`), both
depending on `zpay-core`. `zpay-core` is forbidden from depending on
either adapter. Both mounted in `zpay-runtime`'s Axum router at
namespaced prefixes (`/x402/v2/*`, `/mpp/v1/*`).**

`zpay-core` exposes the lifecycle as typed Rust functions:

- `core::prepare::propose(req: PrepareRequest) -> Result<Preparation, ...>`
- `core::broadcast::submit(prep: PreparationId, raw_tx: RawTxHex) -> Result<SettlementOutcome, ...>`
- `core::oracle::status(payment_id: PaymentId) -> Result<ConfirmationStatus, ...>`
- `core::compliance::verify_poh(token: PohToken, ctx: ComplianceCtx) -> Result<PohClaims, ComplianceError>`
- `core::verify::disclosure(req: DisclosureRequest) -> Result<DisclosureVerdict, ...>`

Each adapter:

- Owns its protocol's request and response codecs.
- Translates wire types into `zpay-core` typed inputs.
- Maps `zpay-core` typed errors into its protocol's error vocabulary.
- Mounts its own Axum router under its prefix.

`zpay-mpp` is feature-gated behind a default-off `mpp` Cargo feature in
v1. Phase 5 of PRD-42 flips it on.

## Rationale

The separate-crate boundary is the only mechanism that makes the
protocol-neutral property a compile-time invariant. If `zpay-mpp` tries
to reach into `zpay-x402` for a shared type, the Cargo dependency graph
fails the build. If `zpay-core` tries to import x402-specific vocabulary
(e.g., `PAYMENT-SIGNATURE` header parsing), the build fails too.

The Axum mounting pattern is a thin composition step in `zpay-runtime`:
each adapter exposes a `pub fn router() -> axum::Router<RuntimeState>`
function; the runtime calls both and merges them at namespaced prefixes.
The runtime knows nothing about the protocols' internal shapes.

## Consequences

Positive:

- The "adding MPP did not modify zpay-core" property is compile-checked
  by Cargo, not just enforced by review.
- An operator can disable an adapter by removing its `pub use` line in
  `zpay-runtime`; the resulting binary cannot serve that protocol even
  with a misconfigured route.
- The OpenAPI spec at `/openapi.json` mounts the adapters' route schemas
  separately; clients consume only the protocols they need.
- New wire protocols (a hypothetical L402, ERC-8004 payment intents, or
  a custom Zcash-native intent format) add a new sibling crate without
  touching the existing ones.

Negative:

- Two `Cargo.toml` files to keep in sync for protocol-versioning bumps.
- Translation glue from wire types to `zpay-core` types is duplicated:
  each adapter has to do its own `MPPPrepareRequest -> PrepareRequest`
  conversion. Mitigated by colocating the codecs inside each adapter.

Neutral:

- The mounting code in `zpay-runtime` is a small composition step. Easy
  to test (mount a fake adapter and assert routing).

## Switch Criteria

Replace this decision when **all** of:

- A third wire adapter appears AND
- The wire shapes diverge enough that the shared lifecycle in
  `zpay-core` starts growing protocol-specific branches AND
- A wire-level polymorphic abstraction (e.g., `WireAdapter` trait
  inside `zpay-core` that all adapters implement) measurably reduces
  duplication.

Until all three fire, the separate-crate model holds.

## Alternatives Considered

### One crate with feature-gated modules

Rejected. The protocol-neutral property is enforceable only at the
import level (`use zpay::x402::*` from a `mpp` module would fail
review), not at the build level. Reviewers miss things; compile-time
enforcement is cheaper.

### Trait-object dispatch inside one crate

Rejected. The Axum routing pattern works against the Rust trait-object
model: routes are typed at construction time. A trait-object adapter
would force `Box<dyn Adapter>` everywhere, losing utoipa's compile-time
OpenAPI generation.

### Parallel reimplementations

Rejected. Duplicates the wallet, broadcast, oracle, and compliance
paths. Doubles the surface to maintain and adds three or four ways to
get the broadcast flow subtly wrong.

## Out of Scope

- A shared "facilitator framework" crate. zpay-core is the framework;
  there is no second consumer for it.
- Wire-level polymorphism (trait `WireAdapter`). Tracked in switch
  criteria.
