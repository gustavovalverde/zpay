# Wallet-stack alignment: adopt the upstream Ironwood stack

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Product | zpay |
| Domain | Wallet plane (`zspend`), chain plane (zinder), upstream binding |
| Started | 2026-07-08 |
| Related | [ADR-0002](../adrs/0002-zally-embedded-as-library.md) (zally embedded), [ADR-0003](../adrs/0003-zinder-as-chain-plane.md) (zinder chain plane), [Upstream platform binding](../architecture/upstream-platform-binding.md), [Orchard tree divergence](../issues/orchard-tree-divergence.md); upstream fixes: unreleased librustzcash Ironwood stack, upstream Orchard bundle-type APIs, incrementalmerkletree fork, zally branch `ironwood-sync-hardening` |

## Problem / why

zpay pins the pre-fix version of every repo in its wallet stack. The faucet
(fauzec) just spent an incident cycle root-causing and fixing a family of
sync, commitment-tree, and Ironwood bugs in exactly the zally, librustzcash,
and shardtree code that `zspend` embeds through ADR-0002. zpay has already hit
one member of that family: `docs/issues/orchard-tree-divergence.md` records a
testnet wallet whose Orchard note-commitment-tree root freezes, so sync aborts
with `WalletError::TreeRootsDiverged` and every Orchard spend is rejected
before signing.

Two things follow. First, zpay should adopt the fixed stack so it stops
carrying the known-broken revisions (the Ironwood truncate wedge, the two
shardtree checkpoint-pruning panics, the repair ladder that never escalates).
Second, adopting the stack is necessary but not sufficient: the bump does not
by itself close the Orchard divergence zpay already filed, and it does not
touch the zpay-owned gaps in how `zspend` drives sync. This plan sequences
both the mechanical bump and the zpay-specific work, and it is explicit about
which pitfalls the bump fixes and which it does not.

## Baseline before this alignment

Pins, from `Cargo.toml` before this alignment:

| Dependency | Pinned rev | Fixed rev this plan targets |
| ---------- | ---------- | --------------------------- |
| zally (`zally-*`) | `4ec16bd` | `8f6e536` |
| librustzcash (`[patch.crates-io]`) | fork `693338fe` | upstream `8e6864a` |
| orchard (`[patch.crates-io]`) | crates.io | upstream `zcash/orchard` `475ef0f` |
| shardtree, incrementalmerkletree | fork `7e79e55` | fork `48b5297` |
| zinder (`zinder-client`, `zinder-proto`) | `6d3d332` | `c604ac3` |

How `zspend` uses the wallet:

- The wallet is zally's `Wallet`, opened at boot with
  `Wallet::builder(...).open_or_create_account(chain, birthday)`
  (`crates/zspend-runtime/src/bootstrap.rs:138`). zpay writes no scan loop,
  no commitment-tree code, and no root verification of its own.
- Sync runs **once, at boot**. The loop at
  `crates/zspend-runtime/src/bootstrap.rs:147` calls `wallet.sync(chain)`
  until `block_count == 0`, then returns. `wallet.sync` appears nowhere else,
  so the wallet is never re-synced while the process serves, and the spend
  handler does not sync before building a spend
  (`crates/zspend-runtime/src/main.rs:931`).
- `zspend` calls `Wallet::sync` directly, not zally's `SyncDriver`. It
  therefore inherits none of the driver's repair ladder, fault classification,
  or park or reprobe behavior. A sync fault fails boot and the container
  restart-loops (`bootstrap.rs:8`).
- The healthcheck is liveness-only: `/healthz` returns a static
  `{"status":"alive"}` with no wallet or sync check
  (`crates/zspend-runtime/src/main.rs:429`). A wedged or stale wallet still
  reports healthy.
- Spends delegate entirely to zally: `SendPaymentPlan::conventional(...)` then
  `wallet.send_payment(plan)` (`crates/zspend-runtime/src/main.rs:931`). There
  is no pool-specific construction, so the wallet spends the conventional
  (Orchard) path. Ironwood is unwired in the wallet today.
- Deployment already carries a restart policy: the per-service Railway configs set
  `restartPolicyType = "ALWAYS"`. zpay does not share fauzec's original
  outage cause (a watchdog exit with no Railway restart).

## The upstream fixes and what each one covers

Landed across the session, all validated end-to-end on live testnet:

- **shardtree checkpoint-pruning (incrementalmerkletree PR #192).** Two
  panics in `prune_excess_checkpoints`, which runs on every frontier insert:
  a folded-leaf case where `clear_flags` panicked with "Tree state
  inconsistent with checkpoints", and an order-dependent flag-clearing case
  that corrupted a retained checkpoint's leaf. Both reproduce most readily on
  a stalled chain (repeated same-position checkpoints).
- **librustzcash upstream Ironwood stack (`8e6864a`).** The pinned upstream
  commit carries the unreleased Ironwood dependency family, target-expiry PCZT
  construction, and Ironwood wallet scanning and storage support. zpay must
  patch the whole family from one commit so zally and zpay resolve one coherent
  type graph.
- **zally `ironwood-sync-hardening` (`4ec16bd` to `8f6e536`).** The latest
  branch head adopts upstream librustzcash, upstream Orchard bundle-type APIs,
  zinder `c604ac3`, and the current incrementalmerkletree fork while preserving
  the prior repair-ladder and subtree-root hardening.

## Plan

### Phase 1: Bump the dependency pins

The bump is a single coordinated change because the crates share one type
graph and one `[patch.crates-io]` set.

- [ ] Bump `zally-*` from `4ec16bd` to `8f6e536` in `Cargo.toml`.
- [ ] Replace the librustzcash fork `[patch.crates-io]` block with upstream
  `github.com/zcash/librustzcash` `8e6864a`, including every
  librustzcash-internal crate zpay or zally reaches.
- [ ] Add the temporary upstream Orchard patch at `github.com/zcash/orchard`
  `475ef0f`, matching the pinned librustzcash commit's bundle-type API needs.
- [ ] Bump shardtree and incrementalmerkletree to
  `gustavovalverde/incrementalmerkletree` `48b5297`. This is mandatory, not
  optional: Cargo `[patch]` sections do not propagate from a git dependency, so
  zpay's own root `[patch]` is the only one that applies.
- [ ] Bump `zinder-client` and `zinder-proto` from `6d3d332` to `c604ac3`
  (zinder `main`), which aligns zpay with the latest zinder dependency and
  Ironwood testkit stack.

Validation gate:

- [ ] `cargo build --workspace` and `cargo test --workspace` pass.
- [ ] `cargo deny check` passes (the pin change touches `deny.toml`'s git
  allowlist if it enumerates revisions).

### Phase 2: Align the deployed zinder (chain plane)

The wallet backfills subtree roots from the zinder deployment behind zpay, not
from the `zinder-client` crate. For today's Orchard-only spend path the older
zinder is adequate, because zally `4ec16bd` falls back to linear scan when a
pool's subtree roots are unavailable. Aligning the deployment matters when
Ironwood is wired (Phase 4 or later).

- [ ] Roll the zinder service behind zpay to an image built from `c604ac3`
  (or later `main`), matching the client and proto bump. Follow the
  blue-green pattern the faucet used: stand up a schema-current store, let it
  reach tip, then cut `ZPAY_CHAIN_SOURCE_URL` and `ZSPEND_CHAIN_SOURCE_URL`
  over.
- [ ] Confirm the store schema version matches the running code; a schema bump
  requires a genesis rebuild rather than an in-place open.

### Phase 3: Re-diagnose the Orchard tree divergence

The pin bump does not conclusively fix the Orchard freeze in
`docs/issues/orchard-tree-divergence.md`. The latest zally stack changes the
baseline by adding upstream librustzcash and zinder alignment, but the Orchard
divergence still needs a direct reproduction against the affected wallet data.

- [ ] Re-run the reproduction from the issue doc against the bumped stack:
  sync a wallet holding the affected legacy Orchard notes from its birthday,
  read the Orchard root via `commitment_tree_roots`, and compare it against
  zinder's `TreeStateAtHeight` Orchard `finalRoot` at scan start.
- [ ] If the root still freezes, the Issue A fix belongs in zally, not zpay.
  File and land it there, following the same method the faucet used: a seeded
  probe that opens the real account against a local zinder and drives the sync
  driver through the divergence heights, so the failure reproduces off a
  fresh wallet DB with the real notes.
- [ ] Fold Issues B and C (empty-tree root returns `Some` instead of `None`;
  `decode_tree_state` defaults a missing frontier) into the same zally change,
  since they share the storage-boundary code.

### Phase 4: Close the zpay-owned sync gaps

Bumping zally does not change how `zspend` drives it. These are zpay refactors,
each mapping to a pitfall the faucet already paid for.

- [ ] **Re-sync while serving.** Move sync off the boot-only path into a
  background task that keeps the wallet tracking tip. A wallet that stops at
  its boot-time height builds spends against a stale anchor as the chain
  advances, which risks expired or invalid transactions. Model it on the
  faucet's continuous wallet-sync task.
- [ ] **Adopt the repair ladder.** Drive sync through zally's `SyncDriver`
  rather than calling `Wallet::sync` directly, so a transient storage fault
  self-heals through rewind and rescan instead of failing boot and
  restart-looping. If a full driver adoption is too large, replicate its
  fault classification and escalation.
- [ ] **Gate spends on freshness.** Replace the liveness-only `/healthz` with a
  readiness signal that reports the wallet's view lag, and refuse to sign a
  spend when the wallet is not fresh, returning an honest `wallet_unavailable`
  rather than signing against stale state. This is the faucet's freshness-gate
  lesson applied to the sign path.
- [ ] **Watch for a wedged sync.** Once sync runs continuously, add a watchdog
  or a sync-progress metric so a stalled wallet is visible and recoverable,
  rather than silently healthy behind `restartPolicyType = "ALWAYS"`.

### Phase 5: Reconcile the binding doc

- [ ] `docs/architecture/upstream-platform-binding.md` records the revisions
  this plan lands, and `Cargo.toml` remains the source of truth for exact pins.

## Decisions (plan-scoped)

The durable binding record lives in `docs/architecture/upstream-platform-binding.md`
and ADR-0002; this plan only sequences the change.

1. **Mirror the exact upstream stack zally pins.** zpay pins
   shardtree and incrementalmerkletree at `48b5297`, librustzcash at
   `8e6864a`, Orchard at `475ef0f`, and zally at `8f6e536`, so the workspace
   resolves one coherent type graph. Do not pick different revisions; the
   crates share a patch set that must be identical across the workspace.
2. **Retire the patch pins together, later.** incrementalmerkletree PR #192
   carries the shardtree fixes upstream. Once it merges and shardtree cuts a
   release, drop the shardtree and incrementalmerkletree `[patch]` from zpay,
   zally, and fauzec in one pass, alongside the librustzcash Ironwood release.
   Until then the fork pins stay.
3. **The Orchard divergence is a zally fix, not a zpay one.** zpay owns no
   commitment-tree code (ADR-0002), so Issues A, B, and C land in zally and
   reach zpay as a pin bump. zpay's role is to reproduce and verify.
4. **Sequence the bump before the refactors.** Phase 1 is a prerequisite for
   Phase 3 and de-risks Phase 4 (the `SyncDriver` adoption targets the
   hardened ladder, not the one that never escalates).

## Validation gates

- [ ] Workspace builds and tests pass on the bumped pins (Phase 1).
- [ ] A wallet syncs from birthday to tip on a fresh volume with no panic and
  no park, against the aligned zinder (Phases 1 and 2).
- [ ] The Orchard divergence reproduction either passes or is converted into a
  filed and fixed zally issue (Phase 3).
- [ ] A spend is refused with `wallet_unavailable` when the wallet is behind
  tip, and accepted when fresh (Phase 4).
