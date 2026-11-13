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
`2e4ad39` (release v0.2.0). This release provides payment-disclosure
production and verification, epoch-pinned native WalletQuery reads, separate
visible and settled tip semantics, typed Zinder transaction submission, and a
single-writer lock on each wallet database file.

## zinder

[github.com/gustavovalverde/zinder](https://github.com/gustavovalverde/zinder)

Role: Zcash indexer. zpay's chain plane.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zinder-client` | `zpay-runtime` | Native WalletQuery broadcast, chain epochs and events, transaction status, and disclosure fetches. |

Pin: `zinder-client` 0.10.0 from crates.io, the same version zally v0.2.0
resolves, so the `RemoteChainIndex` zpay hands to zally's `ZinderSubmitter` is
one type. The pin tracks the native contract revision (8 at 0.10.0): the client
refuses servers below its revision and older clients fail against newer
servers, so cut zpay and zinder over together.

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

zpay uses the released librustzcash cohort that zally v0.2.0 resolves, pinned
exactly in the workspace `Cargo.toml`: `zcash_keys 0.17.0`,
`zcash_primitives 0.31.0`, `zcash_protocol 0.11.0`, `zcash_address 0.14.0`,
`zcash_proofs 0.31.0`, `pczt 0.10.0`, `orchard 0.16.0`, `sapling-crypto 0.9`,
and `zip32 0.3`. Advance the family together with zally so intra-workspace
types stay coherent. There is no `[patch.crates-io]` section.

## Version-pinning policy

- zally pins by git rev in `Cargo.toml`; `deny.toml` allows only that source.
  zinder-client and the librustzcash family pin by released crate version.
  Bumps land in their own PR citing the upstream change and the reason.
- HTTP-only relationships (fauzec, zentity) carry no crate pin; they are
  configured by env var and asserted by integration tests.

## Breaking-change protocol

When a sibling upstream ships a breaking change to a surface zpay depends on,
the rev bump lands in its own PR naming the upstream change. In the other
direction, a breaking change to zpay's own HTTP API ships as a new version
suffix in the wire path (`x402.v3.*`), with the prior version retired on a
sunset schedule.
