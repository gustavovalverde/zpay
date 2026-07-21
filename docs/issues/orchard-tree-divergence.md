# Orchard commitment-tree divergence blocks wallet spends

> **Resolved.** The frozen root was proven byte-for-byte identical to the
> depth-32 root over zinder's three backfilled Orchard subtree caps: the
> defect was the root read introduced in zally `65069ed` (computing over
> backfilled caps spanning unscanned positions), fixed in `04edf80`,
> hardened by `aab23a8`, and closed at the backfill seam by `0b35587`.
> Full evidence and reproduction record:
> [zally#7](https://github.com/gustavovalverde/zally/issues/7). The
> analysis below predates that finding; its candidate mechanisms 1 and 2
> are refuted and 3 is inverted by the proof.

## Summary

A testnet wallet holding legacy Orchard notes cannot spend. Sync aborts with `WalletError::TreeRootsDiverged`: the wallet's Orchard note-commitment-tree root never tracks the chain, while its Sapling tree tracks correctly. Without a converged Orchard tree the wallet cannot build a valid anchor, so every Orchard spend is rejected before it is signed.

The defect is wallet-side, in zally's Orchard commitment-tree construction. The pinned librustzcash Ironwood fork (`235d581`) is not the cause. Its scan appends legacy Orchard actions to the Orchard tree unconditionally, with no NU6.3 gating, and Ironwood is a separate parallel pool. This document records the fileable zally issues and the librustzcash exoneration, both grounded in the source at the pinned revisions.

## Environment

| Component | Pin | Notes |
|-----------|-----|-------|
| zally | `ec5b213` | `zally-wallet`, `zally-storage`, `zally-chain` |
| zinder | `74abb4b` | `zinder-client`, `zinder-proto` (Ironwood mains) |
| librustzcash | `235d581` | `[patch.crates-io]`, PR #2539 (Ironwood scan/storage) + PR #2412 (`target_expiry_height`) |
| zpay | `af63630` | integration point where the mismatch surfaces |

Chain: Zcash testnet. NU6.3/Ironwood activates at height 4,133,000. The affected wallet's notes are pre-activation legacy Orchard at heights 4,056,276 and 4,057,384. Account birthday is height 4,050,200. The unified address carries `receiver_flags = 13` (transparent + Sapling + Orchard).

## Symptom

- Sync returns `WalletError::TreeRootsDiverged`.
- The Orchard commitment-tree root does not advance to match the chain, while Sapling matches at every scanned height.
- The behaviour persists across a full `wallet.db` wipe and a fresh rescan from the birthday, so it is not local database corruption.
- The observed frozen Orchard root is a real non-empty value. It matches neither the canonical empty-tree root nor zinder's Orchard frontier at the birthday height (see Evidence), so the Orchard tree receives some state and then stops advancing.

## The chain feed is correct

zinder serves a valid, non-empty Orchard frontier at the wallet birthday. `TreeStateAtHeight` at height 4,050,200 returns Orchard `finalRoot = 873948679049069b6a463703c5f776d1c6f71eee96542f41066e51c63c00ff1c` alongside a populated `finalState`, and `orchardCommitmentTreeSize` advances normally (236,668 at the current tip). zinder also serves the legacy Orchard `actions` (lightwalletd `CompactBlock` field 6) that carry this wallet's notes, and it is consensus-verified canonical against zebra. The raw tree-state and note feeds are therefore available and correct; the fault is in how the wallet consumes and appends them.

## librustzcash `235d581` is not the cause

At the pinned revision the scan path builds the Orchard tree from scanned legacy actions, and the reference storage path appends those commitments to the Orchard `ShardTree`. Both stages are Orchard-complete with no post-activation skip.

- Scan emits Orchard leaves unconditionally. `scan_block_with_runners` reads each `CompactTx`'s `actions` (legacy Orchard, proto field 6), trial-decrypts under `OrchardDomain`, and pushes one `MerkleHashOrchard::from_cmx(&output.cmx())` per action into `orchard_note_commitments`. This is gated only by the `orchard` cargo feature, not by any branch-id or NU6.3 conditional (`zcash_client_backend/src/scanning/compact.rs:412-447`).
- Ironwood is a separate parallel pool, not a replacement. Ironwood commitments come from `ironwood_actions` (proto field 9) into a distinct tree and distinct `ScannedBundles` (`compact.rs:450-485`, `compact.rs:528-543`). Post-activation blocks do not divert legacy Orchard actions away from the Orchard tree.
- The reference storage path appends the Orchard leaves. `put_blocks` collects `block.into_commitments().orchard`, builds subtrees via `build_subtrees::<_, ORCHARD_SHARD_HEIGHT>` seeded at the caller-supplied `ChainState` Orchard frontier, and calls `with_orchard_tree_mut(|orchard_tree| update_tree("Orchard", from_state.final_orchard_tree(), ..., orchard_tree, ...))` (`zcash_client_backend/src/data_api/ll/wallet.rs:467-508`, `wallet.rs:584-604`).
- Inconsistent inputs fail loudly, not silently. A mismatch between the supplied frontier and a block's reported tree size returns `PutBlocksError::NonSequentialBlocks` (`wallet.rs:237-246`), and a missing `orchardCommitmentTreeSize` against present actions underflows to `TreeSizeInvalid` (`compact.rs:690-706`). A silently frozen tree is inconsistent with using this path as written.
- The only NU6.3 default-skip is Ironwood, not Orchard. On the `WalletCommitmentTrees` trait, `with_orchard_tree_mut` and `put_orchard_subtree_roots` are required methods, while `with_ironwood_tree_mut` has a default returning `Ok(None)` (`zcash_client_backend/src/data_api.rs:3740-3778`). Only Ironwood can be silently dropped by a non-overriding backend, and this wallet's notes are pre-Ironwood.

librustzcash HEAD (`9eb1f86`) sits ahead of the pin but does not change this conclusion for the Orchard scan path.

## Root cause locus

zally owns the Orchard `ShardTree` population. It reads the Orchard root straight out of librustzcash-owned shardtree storage and never computes the tree itself: `commitment_tree_roots` returns `orchard = with_orchard_tree_mut(|tree| tree.root_at_checkpoint_depth(Some(0)))...map(|node| node.to_bytes())` (`crates/zally-storage/src/sqlite.rs:701-726`). The tree is written only through librustzcash's `WalletWrite` inside `storage.scan_blocks` (`crates/zally-wallet/src/sync.rs:1558`) and through `put_orchard_subtree_roots` (`sqlite.rs:653`).

zally does wire Orchard into the pipeline: the chain source maps Orchard to a supported pool and rejects only Ironwood (`crates/zally-chain/src/zinder_source.rs:308-314`); `backfill_subtree_roots` iterates both Sapling and Orchard (`sync.rs:1461-1497`); and the scan-start frontier is carried whole into the scan via `from_state` (`sync.rs:1361-1364`). So the Orchard tree is fed a frontier and leaves, yet its root freezes at a non-empty value that never reaches the chain root.

That narrows the fault to zally's Orchard append and checkpoint seam: the scanned Orchard commitments and the backfilled subtree roots are not reconstructing the chain Orchard root the way the equivalent Sapling path does. The single measurement that pins the mechanism is whether the frozen root equals the Orchard frontier root that zinder serves at scan start (see Issue A).

---

## Issue A (zally, primary): Orchard commitment-tree root does not track the chain, blocking all Orchard spends

**Repo:** zally
**Title:** Orchard commitment-tree root frozen while Sapling tracks the chain, blocking spends with `TreeRootsDiverged`

**Impact.** A wallet with legacy Orchard notes cannot spend them. Sync aborts with `WalletError::TreeRootsDiverged` because the Orchard tree root never matches the chain, even though the same wallet's Sapling tree tracks correctly and the light-wallet server serves a valid Orchard frontier and the relevant Orchard actions.

**Observed behaviour.** The Orchard root read by `commitment_tree_roots` (`crates/zally-storage/src/sqlite.rs:701-726`) stays constant at a non-empty value across every scanned height and across a `wallet.db` wipe and rescan. Sapling advances normally over the same range.

**Scope.** The scanned Orchard commitments come from librustzcash correctly (`zcash_client_backend/src/scanning/compact.rs:412-447`), and zinder serves a correct non-empty Orchard frontier at the birthday. The gap is between those inputs and zally's populated Orchard `ShardTree`. Candidate mechanisms:

1. The scanned `ScannedBlock.orchard()` commitments are not appended through `with_orchard_tree_mut` and `update_tree("Orchard", ...)` the way Sapling is, so per-block Orchard appends never land.
2. The Orchard `ChainState` frontier passed to the scan does not match the real Orchard tree size at scan start, so appends build from the wrong position and the reconstructed root diverges.
3. The Orchard subtree-root backfill (`sync.rs:1461-1497`) does not complete the region below the birthday, leaving the tree unable to reconstruct the full chain root.

**Decisive measurement to pin the mechanism.** Compare the frozen wallet Orchard root against the Orchard `finalRoot` that zinder's `TreeStateAtHeight` returns at scan start (`873948679049069b6a463703c5f776d1c6f71eee96542f41066e51c63c00ff1c` at height 4,050,200). If they are equal, the frontier was seeded but per-block appends never ran (mechanism 1). If they differ, the frontier itself is wrong (mechanism 2). This measurement selects the fix location.

**Suggested direction.** Confirm zally's `WalletWrite`/`WalletCommitmentTrees` implementation drives Orchard exactly as the librustzcash reference `put_blocks` does, rather than treating Orchard like Ironwood's `Ok(None)` default. Ensure the `from_state` Orchard frontier matches the tree size at scan start and that `put_orchard_subtree_roots` covers the pre-birthday region.

## Issue B (zally, latent): `verify_tree_roots` contradicts its documented empty-tree skip

**Repo:** zally
**Title:** `verify_tree_roots` hard-faults a scanned-but-empty Orchard tree, contradicting its empty-tree skip contract

**Impact.** A wallet whose Orchard tree is legitimately empty at a checkpoint (Sapling correct, no Orchard leaves yet) faults sync with `TreeRootsDiverged` and no escape hatch, contradicting the documented contract that empty trees are skipped.

**Mechanism.** `commitment_tree_roots` returns `Some(empty_root)` rather than `None` for a scanned-but-empty Orchard tree, because scanning creates a checkpoint every block, so `root_at_checkpoint_depth(Some(0))` sees that checkpoint (`crates/zally-storage/src/sqlite.rs:715-722`). `verify_tree_roots` skips only when both pools are `None`; its success arm passes when `sapling_match != Some(false) && orchard_match != Some(false)`, and its fallthrough diverges whenever either pool is `Some(false)` (`crates/zally-wallet/src/sync.rs:1672-1712`). A Sapling-correct, Orchard-empty state yields `orchard_match == Some(false)` and hits the divergence arm, contradicting the doc comment at `sync.rs:1631-1633`.

**Suggested direction.** At the storage boundary return `None` for a provably empty (zero-leaf) tree so the existing `(None, ...)` guard skips it, applied symmetrically to Sapling. Do not relax the non-empty mismatch arm: a genuinely divergent non-empty root must still fault, since that is the case in Issue A. Optionally gate the Orchard comparison on whether the wallet actually holds Orchard notes.

## Issue C (zally, latent): `decode_tree_state` silently defaults a missing Orchard frontier

**Repo:** zally
**Title:** `decode_tree_state` uses `unwrap_or_default()` for a missing Orchard/Ironwood `finalState`, silently seeding an empty frontier

**Impact.** When a tree-state response omits the Orchard (or Ironwood) `finalState`, zally seeds an empty frontier into the scan instead of faulting. For a wallet whose birthday precedes an Orchard tree in the served tree-state, this quietly produces a wrong tree that the root check later blames, hiding the true cause.

**Mechanism.** `decode_tree_state` fills `orchard_tree` from the `/orchard/commitments/finalState` JSON pointer but falls back to `unwrap_or_default()` (`crates/zally-chain/src/zinder_source.rs:379-383`). zinder omits the Orchard tree entirely from the tree-state at pre-activation heights (confirmed: `TreeStateAtHeight` at height 1,000,000 returns no Orchard `finalRoot`), so this default path is reachable.

**Suggested direction.** Replace `unwrap_or_default()` with a hard `MalformedCompactBlock` error at post-Orchard-activation heights, so an un-fed frontier faults loudly rather than seeding a silent empty tree. Apply the same to `ironwood_tree` once Ironwood is wired.

## librustzcash `235d581`: not a bug, optional hardening only

No librustzcash change is required for this divergence. The scan emits and appends legacy Orchard leaves as shown in "librustzcash `235d581` is not the cause". If defensive hardening is nonetheless wanted, `put_blocks` could assert that a non-empty `orchard_commitments` input advanced the Orchard tree size, converting a silently unadvanced Orchard tree into an explicit error. This guards backends that bypass `with_orchard_tree_mut`; it does not change the reference path, which already errors on frontier mismatch.

---

## Evidence

**Live tree-state (zinder `74abb4b`, testnet, `TreeStateAtHeight`).**

- Height 4,050,200 (birthday): Orchard `finalRoot = 873948679049069b6a463703c5f776d1c6f71eee96542f41066e51c63c00ff1c`, Sapling `finalRoot = 2809bfce6bb818928f263252f9c72ca149d8a6c32913709cd9f2e11fd6358300`, with populated `finalState` for both.
- Chain metadata: `orchardCommitmentTreeSize = 236668`, `saplingCommitmentTreeSize = 370762` at the current tip (height 4,147,234).
- Height 1,000,000 (pre-NU5): no Orchard `finalRoot` in the tree-state, confirming the absent-`finalState` case relevant to Issue C.

**Reported wallet root.** `78bdaf718b1d566946420dbc7d755c35e31587d600767d6cda46154429b77403`, constant across scanned heights. It does not equal the birthday Orchard frontier above in either byte order, and it is not the canonical empty Orchard tree root.

**Historical reproduction.** These commands and ports describe the
pre-cutover deployment captured by this resolved incident. They are evidence,
not current WalletQuery operating instructions.

1. Fetch a compact block carrying the Orchard actions and confirm the pool contents:
   `grpcurl -plaintext 127.0.0.1:19102 zinder.v1.wallet.WalletQuery/CompactBlock -d '{"height":4056276}'`, base64-decode `payloadBytes`, then `protoc --decode=cash.z.wallet.sdk.rpc.CompactBlock` and count `actions` (Orchard, field 6) versus `outputs` (Sapling, field 5).
2. Fetch the Orchard frontier at scan start:
   `grpcurl -plaintext 127.0.0.1:19102 zinder.v1.wallet.WalletQuery/TreeStateAtHeight -d '{"height":4050200}'`, base64-decode `payloadBytes`, read `orchard.commitments.finalRoot`.
3. Cross-check against consensus with zebra `z_gettreestate` at the same heights (zebra returns roots in reversed byte order).
4. Sync the wallet from the birthday and read the Orchard root via `commitment_tree_roots`; observe it frozen while Sapling advances.
