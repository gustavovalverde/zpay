# ADR-0001: Workspace and crate boundaries

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Project structure |
| Related | [Public interfaces](../architecture/public-interfaces.md), [Operational surfaces](../architecture/operational-surfaces.md), [PRD-42 (zentity)](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md) |

## Context

zpay is a payments-protocol facilitator that embeds a wallet library
(zally), calls an indexer (zinder), and exposes two HTTP wire protocols
(x402 v2, MPP). Multiple boundary candidates exist:

- One crate (everything in `zpay`). Smallest unit; loses the wire-adapter
  isolation that proves the core is protocol-neutral.
- Two crates (core, runtime). Bigger but still conflates wire adapters
  with the core.
- Six crates (core, store, x402, mpp, runtime, testkit). Largest unit;
  highest cost.
- A shared `zcash-*` crate spanning multiple sibling projects. Pre-supposes
  a second consumer.

The PRD-42 analysis applied YAGNI and rule-of-three to the shared-crate
question: no second sibling consumer exists for any of the candidate
shared types (capability strings, bearer auth, money types). A shared
crate now forces premature design.

The wire-adapter isolation question is load-bearing: PRD-42 Decision 2 and
M4 exit criterion both require that adding `zpay-mpp` does not modify
`zpay-core`. Two separate crates make that property compile-checkable; one
crate with feature-gated modules makes it only test-checkable.

## Decision

**Six-crate workspace. No shared `zcash-*` crates yet.**

| Crate | Boundary | Owns |
|---|---|---|
| `zpay-core` | Library. | Domain types, prepare / oracle / broadcast / compliance modules, capability strings. |
| `zpay-store` | Library. libSQL only. | Prepared-tx cache, settlement ledger, bearer-key-hash table, schema migrations. |
| `zpay-x402` | Library. Axum router only. | x402 v2 route handlers, DPoP middleware, x402-specific request/response codecs. |
| `zpay-mpp` | Library. Axum router only. Feature-gated. | MPP route handlers. Stubbed in M0; implemented in M4. |
| `zpay-runtime` | Binary. | Composition root, env-driven config, ops listener, tracing, signal handling, OpenAPI generation. |
| `zpay-testkit` | Library. Test-only. | `require_live()` gates, mock chain source, mock submitter, settlement fixtures. |

Internal dependencies:

```text
zpay-runtime --> zpay-core, zpay-store, zpay-x402, zpay-mpp (feature-gated)
zpay-x402    --> zpay-core
zpay-mpp     --> zpay-core
zpay-core    --> zpay-store
zpay-store   --> (no zpay internal deps; libSQL only)
zpay-testkit --> zpay-core, zpay-store (dev-only consumers)
```

External dependencies:

- `zpay-core` depends on `zally-core`, `zally-wallet`, `zinder-client` (all
  pinned by git rev in the workspace `Cargo.toml`).
- `zpay-compliance` (inside `zpay-core`) depends on `jsonwebtoken` and
  `reqwest` for SD-JWT-VC verification.
- No crate depends on `tonic` directly. zpay does not expose a gRPC
  surface; it consumes zinder's via `zinder-client`.

## Rationale

The six-crate layout is the smallest unit that makes the protocol-neutral
property a compile-time invariant. Adding a future `zpay-mpp` cannot
reach into `zpay-x402`'s code because they are siblings in `zpay-runtime`,
not children of a shared module. The rule that wire adapters must depend
only on `zpay-core` (not on each other and not on `zpay-runtime`) is
enforced by the Cargo dependency graph.

`zpay-testkit` is separate because it depends on dev-only crates (mocks,
fixtures) that the production binary must not link. Mixing it into
`zpay-core` would pull `mockall` into production builds.

`zpay-store` is separate because libSQL has heavy build dependencies
(SQLite, rustls) that should not appear in `zpay-x402` or `zpay-mpp`
when consumed standalone (e.g., from a test harness).

## Consequences

Positive:

- Wire-adapter isolation is compile-checkable.
- `cargo build -p zpay-x402` builds the wire adapter alone; useful when
  iterating on x402 routes without recompiling libSQL.
- Future shared types are easy to spot: when two crates start importing
  the same types from `zpay-core`, the boundary is right; when they need
  the same types from outside `zpay-core`, that is the signal to consider
  a new shared module inside `zpay-core` (not a new shared crate).

Negative:

- Six `Cargo.toml` files to keep in sync. Mitigated by the workspace
  `[workspace.dependencies]` table.
- Initial setup overhead vs a one-crate scaffold.

Neutral:

- The pattern matches fauzec (10 crates) and zinder (12 crates) more than
  zally (7 crates). All three sit in the same workspace style.

## Switch Criteria

Replace this decision when **all** of:

- A third wire adapter beyond x402 and MPP becomes credible (suggesting
  the per-adapter crate pattern is the wrong factoring).
- The maintenance cost of six `Cargo.toml` files measurably slows shipping
  cadence.
- A shared `zcash-*` crate becomes viable because a second non-zpay
  consumer of one of zpay's types appears.

## Alternatives Considered

### One-crate scaffold

Rejected. Loses the wire-adapter compile-time isolation. Adding `zpay-mpp`
would require feature-gated modules inside one crate, which lets MPP
reach into x402 code by mistake; the violation only surfaces during
integration test runs.

### Three-crate split (core, server, store)

Rejected. The "server" crate would mix x402 routes, MPP routes, and the
Axum binary. The same isolation problem as the one-crate scaffold but at
a smaller scale.

### Shared `zcash-*` crates

Rejected for v1. Rule-of-three has not fired: bearer-auth has only one
consumer (fauzec), capability strings have only one Rust consumer
(zinder), and money types are already in `zally-core`. Premature
extraction creates cross-project coupling that costs more to revert than
to maintain locally.

## Out of Scope

- Mainnet operator-wallet custody crate (would be `zpay-custody` or
  similar). Tracked in [product-requirements.md §Open Questions](../product-requirements.md#open-questions).
- A `zpay-mcp` crate exposing zpay as an MCP server. zentity remains the
  MCP gateway per PRD-42 Decision 8.
