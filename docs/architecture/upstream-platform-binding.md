# Upstream Platform Binding

zpay depends on sibling Zcash-stack repositories and on unreleased Zcash
workspace commits. This document specifies what zpay pins, why, and what each
upstream is expected to provide.

## zally

[github.com/gustavovalverde/zally](https://github.com/gustavovalverde/zally)

Role: Rust wallet library. zpay's only library-shaped wallet dependency.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zally-core` | `zpay-core`, `zspend-*` | `Zatoshis`, `TxId`, `Memo`, `Network`, `SignedPayload` newtypes. |
| `zally-chain` | `zpay-core`, `zpay-runtime`, `zspend-runtime` | `Submitter`, `ChainSource`, `ZinderChainSource`, `parse_transaction_expiry_height`. |
| `zally-keys` | `zspend-runtime` | `SeedSealing` trait, `AgeFileSealing`, `SealingPosture`. |
| `zally-storage` | `zpay-demo`, `zpay-e2e`, `zspend-runtime` | `Sqlite` wallet database adapter. |
| `zally-wallet` | `zspend-runtime` | `Wallet`, `SyncDriver`, transaction proposal and signing. |
| `zally-testkit` | tests | fixtures and mock chain sources. |

Pin: workspace `Cargo.toml` pins one git rev for every zally crate, currently
`345b370`. This revision provides payment-disclosure production and
verification, epoch-pinned native WalletQuery reads, separate visible and
settled tip semantics, and typed Zinder transaction submission.

## zinder

[github.com/gustavovalverde/zinder](https://github.com/gustavovalverde/zinder)

Role: Zcash indexer. zpay's chain plane.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zinder-client` | `zpay-runtime` | Native WalletQuery broadcast, chain epochs and events, transaction status, and disclosure fetches. |

Pin: `zinder-client` at git rev `2a4b982`. Bumps land in their own PR.

Wallet deployments that claim Ironwood subtree-root coverage must observe the
`wallet.read.subtree_roots_ironwood_v1` capability from zinder `ServerInfo`.

`ZPAY_CHAIN_SOURCE_URL` is the single zinder endpoint for broadcast, chain
observation, tip reads, and disclosure transaction fetch. There is no separate
fallback oracle. zpay reaches zinder directly in every supported deployment
(see [ADR-0003](../adrs/0003-zinder-as-chain-plane.md)).

The endpoint must be served by Zinder's native query process backed by a live
projector and canonical ingest process. Zinder's current Railway target is
ingest-only and is not a valid zpay chain source; no port substitution can make
that topology wallet-serving.

**Store schema.** The pinned zinder is at artifact schema version 20
(`CURRENT_ARTIFACT_SCHEMA_VERSION`). A zinder store written below schema 20 is
incompatible and must be wiped and resynced; follow zinder's own store-reset
runbook.

## librustzcash family

zpay pins the unreleased upstream librustzcash workspace
(`github.com/zcash/librustzcash`, rev `8e6864a`) through `[patch.crates-io]`.
Every librustzcash-internal crate is patched from the same commit so
intra-workspace types stay coherent: `zcash_client_backend`,
`zcash_client_sqlite`, `zcash_encoding`, `equihash`, `pczt`, `zcash_protocol`,
`zcash_primitives`, `zcash_keys`, `zcash_address`, `zcash_transparent`,
`zcash_proofs`, and `zip321`.

The upstream commit carries the unreleased Ironwood/NU6.3 dependency family,
target-expiry PCZT construction, and Ironwood wallet scanning and storage
support.

`orchard` is patched to `github.com/zcash/orchard` rev `475ef0f` because the
pinned librustzcash commit depends on unreleased bundle-type APIs.

Drop the patch set once upstream releases ship these APIs. `deny.toml` allows
these sources under `[sources] allow-git`, alongside the zally and zinder
sources.

`shardtree` and `incrementalmerkletree` are pinned to the
`github.com/gustavovalverde/incrementalmerkletree` fork at `48b5297`, matching
zally's anchor-retention APIs.

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
- The unreleased librustzcash and Orchard pins live under `[patch.crates-io]`;
  `deny.toml` allows their sources.
- HTTP-only relationships (fauzec, zentity) carry no crate pin; they are
  configured by env var and asserted by integration tests.

## Breaking-change protocol

When a sibling upstream ships a breaking change to a surface zpay depends on,
the rev bump lands in its own PR naming the upstream change. In the other
direction, a breaking change to zpay's own HTTP API ships as a new version
suffix in the wire path (`x402.v3.*`), with the prior version retired on a
sunset schedule.
