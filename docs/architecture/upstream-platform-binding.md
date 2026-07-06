# Upstream Platform Binding

zpay depends on sibling Zcash-stack repositories and on a fork of the
librustzcash workspace. This document specifies what zpay pins, why, and what
each upstream is expected to provide.

## zally

[github.com/gustavovalverde/zally](https://github.com/gustavovalverde/zally)

Role: Rust wallet library. zpay's only library-shaped wallet dependency.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zally-core` | `zpay-core`, `zspend-*` | `Zatoshis`, `TxId`, `Memo`, `Network`, `SignedPayload` newtypes. |
| `zally-chain` | `zpay-core`, `zpay-runtime` | `Submitter`, `ChainSource`, `ZinderChainSource`. |
| `zally-keys` | `zspend-runtime` | `SeedSealing` trait, `AgeFileSealing`, `SealingPosture`. |
| `zally-storage` | `zpay-core` | `parse_v5_expiry_height` (the settle expiry gate). |
| `zally-wallet` | `zspend-runtime` | `Wallet`, transaction proposal and signing. |
| `zally-testkit` | tests | fixtures and mock chain sources. |

Pin: workspace `Cargo.toml` pins one git rev for every zally crate,
currently `ab9cba7` (Ironwood/NU6.3-aware). Bumps land in their own PR.

## zinder

[github.com/gustavovalverde/zinder](https://github.com/gustavovalverde/zinder)

Role: Zcash indexer. zpay's chain plane.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zinder-client` | `zpay-runtime` | `RemoteChainIndex::broadcast_transaction`, `ChainEvents`, tip reads, disclosure fetch. |
| `zinder-proto` | `zpay-runtime` | the generated protobuf types. |

Pin: `zinder-client` and `zinder-proto` at git rev `429db6a`. Bumps land in
their own PR.

The broadcast endpoint is `ZPAY_CHAIN_SOURCE_URL`; the disclosure-fetch
explorer plane is `ZPAY_EXPLORER_URL`. Both point at a zinder deployment;
there is no separate fallback oracle. zpay reaches zinder directly in every
supported deployment (see [ADR-0003](../adrs/0003-zinder-as-chain-plane.md)).

**Store schema.** The pinned zinder is at artifact schema version 12
(`CURRENT_ARTIFACT_SCHEMA_VERSION`). A zinder store written below schema 12 is
incompatible and must be wiped and resynced; follow zinder's own store-reset
runbook.

## librustzcash fork family

zpay pins the entire librustzcash workspace to a fork
(`github.com/gustavovalverde/librustzcash`, rev `235d581`) through
`[patch.crates-io]`. Every librustzcash-internal crate is patched from the
same commit so intra-workspace types stay coherent: `zcash_client_backend`,
`zcash_client_sqlite`, `pczt`, `zcash_protocol`, `zcash_primitives`,
`zcash_keys`, `zcash_address`, `zcash_transparent`, `zcash_proofs`, and
`zip321`.

The fork tracks the unreleased Ironwood/NU6.3 dependency family and carries
two patches on top of upstream `main`:

- the `target_expiry_height` argument on `create_pczt_from_proposal`
  ([librustzcash PR 2412](https://github.com/zcash/librustzcash/pull/2412));
- wallet scanning and storage for the Ironwood shielded pool
  ([librustzcash PR 2539](https://github.com/zcash/librustzcash/pull/2539)).

Drop the patch set once upstream releases ship both. `deny.toml` allows these
fork sources under `[sources] allow-git`, alongside the zally and zinder
sources.

**Validator requirement.** Ironwood/NU6.3 is served by Zebra only; it is the
only full validator past NU6.3. A zinder deployment behind zpay must index a
Zebra node at that network upgrade.

## fauzec

[github.com/gustavovalverde/fauzec](https://github.com/gustavovalverde/fauzec)

Role: Testnet faucet. No Rust dependency. The relationship is operational:
the `zpay-e2e` validator claims TAZ from fauzec to fund agent-payment flows.

## zentity

[github.com/gustavovalverde/zentity](https://github.com/gustavovalverde/zentity)

Role: Identity issuer. It mints the DPoP-bound `payment_authorization` access
tokens the wallet runtime (`zspend-runtime`) verifies, and it is the sole
spend-policy authority for the agent-signed path (Proposal-0003 D-1/D-2).

`zpay-runtime` itself has no zentity dependency: it validates no PoH token and
runs no JWKS client (see
[ADR-0008](../adrs/0008-compliance-authority-placement.md)). The wallet
runtime consumes zentity's JWKS and revocation delta endpoint; see
`zspend-runtime`'s configuration.

## Version-pinning policy

- Sibling Rust workspaces (zally, zinder) pin by git rev in `Cargo.toml`.
  Bumps land in their own PR citing the upstream change and the reason.
- The librustzcash fork pins by git rev under `[patch.crates-io]`; `deny.toml`
  allows its source.
- HTTP-only relationships (fauzec, zentity) carry no crate pin; they are
  configured by env var and asserted by integration tests.

## Breaking-change protocol

When a sibling upstream ships a breaking change to a surface zpay depends on,
the rev bump lands in its own PR naming the upstream change. In the other
direction, a breaking change to zpay's own HTTP API ships as a new version
suffix in the wire path (`x402.v3.*`), with the prior version retired on a
sunset schedule.
