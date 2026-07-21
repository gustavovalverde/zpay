# ADR-0009: Settlement lifecycle and finality semantics

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Settlement lifecycle, confirmation oracle, reorg handling |
| Related | [ADR-0003](0003-zinder-as-chain-plane.md), [ADR-0004](0004-libsql-prepared-tx-cache.md), [ADR-0006](0006-facilitator-trust-boundary.md), [Facilitator plane](../architecture/facilitator-plane.md) |

## Context

An agent that pays for a resource needs one honest answer: is this payment
safe to act on yet. Zcash gives no single-bit answer. A transaction sits in
the mempool, gets mined, accrues confirmations, and can still lose its block
to a reorg. A facilitator that reports "confirmed" on the first mined block,
or on an N-confirmation depth heuristic, will occasionally tell an agent to
release goods against a payment that later vanishes.

Two properties have to coexist in the status a caller reads:

- **A milestone for UX confidence.** Most callers want a "deep enough to act
  in practice" signal, tunable per network. This is a depth heuristic and it
  is allowed to be wrong under a deep reorg.
- **A guarantee of immutability.** Some callers want the point past which no
  reorg can move the payment. This cannot be a depth count; it has to come
  from the chain plane's own reorg-window accounting.

zinder exposes both a visible tip and a settled tip, where the settled tip
is the server-derived reorg-window settlement watermark (ADR-0003). zpay's
job is to project those two heights, plus its own ledger,
into a status a caller can trust and to make every regression observable.

The prepared row is removed on a successful settle, so the ledger has to
carry enough state to lapse an unmined payment on its own. Migration 0002
adds that state.

## Decisions

### Statuses regress; they are not a monotonic ladder

`PaymentStatus` (`crates/zpay-core/src/status.rs`) is `Awaiting`,
`Broadcast`, `Mined`, `Final`, `Failed`, `NeverIssued`, `Expired`. A mined
payment can return to `Broadcast` when a reorg drops its block. Only
`Failed`, `NeverIssued`, and `Expired` are terminal by status
(`PaymentStatus::is_terminal`). `Mined` and `Final` are not terminal.

### `Final` is a UX milestone, not immutability

`Final` means the oracle observed `confirmation_count >= ZPAY_FINALITY_DEPTH`
(`DEFAULT_FINALITY_DEPTH` is 3; mainnet operators raise it via
`ZPAY_FINALITY_DEPTH`). It is a depth heuristic. A reorg deeper than the
finality threshold still returns the payment to `Broadcast`. `Final` does not
close a live stream and does not promise the payment cannot move.

### The settled tip is the immutability authority

A snapshot carries a `settled` boolean, true once the payment's
`mined_block_height` is at or below the chain's settled tip
(`ChainStatusView::is_settled_at`). This is the only immutability signal. It
uses no depth heuristic: it is a direct comparison against the chain plane's
reorg-window ceiling. Once `settled` is true, no reorg can move the payment.

### Reorg downgrade clears mined state and records the regression

When the chain plane stops reporting a mined payment as mined, the ledger
row downgrades: `mined_block_height` clears, `confirmation_count` zeroes,
`reorg_count` increments, and `last_reorged_at` stamps the wall-clock second
of the downgrade. The status projection then reports `Broadcast` (or
`Expired`, if the expiry height has since lapsed).

Downgrades fire from two independent sources, each labeled on the
`zpay_reorg_downgrades_total{source}` counter:

- **`chain_event`.** The chain-events subscription
  (`crates/zpay-runtime/src/chain_events.rs`) consumes `ChainReorged`
  envelopes, reads the reverted block range, and downgrades every ledger row
  whose mined height falls inside it (`downgrade_reorged_range`). The task
  runs a full reconciliation sweep on startup and again on every resume
  cursor expiry.
- **`poll`.** The 60-second confirmation poll downgrades a row the oracle
  reports as `NotFound`, `ConflictingChain`, or back in the mempool after
  having been mined (`downgrade_on_reorg`).

### An unmined payment lapses to `Expired` at its expiry height

The prepared `expiry_height` is carried onto the ledger at settle time
(migration 0002). An unmined success-kind row whose `expiry_height` is at or
below the visible tip (`ChainStatusView::is_lapsed_at`) projects to
`Expired`, which is terminal. This covers a broadcast that was reorged out
and never re-mined, and a payment that never confirmed. A prepared-but-never-
settled row whose wall-clock TTL passed also reads `Expired`.

### The SSE stream closes on immutable success or a terminal status, never on `Final`

`GET /zpay/v1/payments/{payment_id}/events` closes after emitting the first
snapshot for which `PaymentStatusSnapshot::stream_closed` holds: `settled` is
true, or the status is terminal (`Failed`, `NeverIssued`, `Expired`). A
`Final` snapshot that is not yet settled keeps the stream open so a later
reorg downgrade still reaches the subscriber.

### `reorg_count` and `settled` are wire-visible

Both `GET /zpay/v1/payments/{payment_id}` and the SSE snapshot carry
`reorg_count` and `settled`. `reorg_count` lets a caller and an operator see
how many times a payment regressed; `settled` is the immutability bit a
caller keys the release decision on.

## Rationale

Depth-based finality and reorg-window immutability answer different
questions, so they are different fields. A single "confirmed at N" flag would
either be too eager (acts before immutability) or too conservative (waits
past the point a UX wants to show success). Splitting them lets the milestone
be tunable and fallible while the guarantee stays exact.

The settled tip is authoritative because the chain plane owns reorg-window
accounting; zpay reconstructing it from a depth count would duplicate that
logic and drift from it. Reading `mined_block_height <= settled_tip` is a
single comparison with no heuristic.

Two downgrade sources exist because the subscription is fast but can miss
windows (cursor expiry, restart) and the poll is slow but complete. The poll
plus a startup-and-cursor-expiry sweep is the backstop that makes the
subscription's gaps recoverable, and both write the same regression fields so
a caller cannot tell which path corrected the record.

Closing the stream on `Final` would be the classic bug: a subscriber sees
`final`, disconnects, and never learns the block reorged out. Closing on
`settled` (or a terminal status) is the only close condition that cannot
strand a subscriber before immutability.

## Consequences

Positive:

- A caller has one exact bit (`settled`) to gate irreversible actions on, and
  a separate tunable milestone (`Final`) for optimistic UX.
- Every regression is observable through `reorg_count`, `last_reorged_at`,
  and the `zpay_reorg_downgrades_total{source}` counter.
- A payment that reorgs out and never re-mines does not hang; it lapses to
  the terminal `Expired` once the visible tip reaches its expiry height.

Negative:

- A payment can show `final` and later read `broadcast` again. Callers that
  treat `final` as terminal will be wrong under a deep reorg; the contract
  makes them key on `settled` instead.
- The status projection depends on a fresh chain view. With a stale or
  unknown chain view the projection fails open: no row reads `settled` and no
  unmined row lapses. Readiness reports the chain-cache age so an operator
  sees the staleness (ADR-0003, operational surfaces).

Neutral:

- `reorg_count` is monotonic per payment and never resets; a caller reads it
  as "how many times did this payment regress," not as a live state.

## Switch Criteria

Revisit when **any** of:

- The chain plane changes how it exposes the settled tip, so
  `is_settled_at` no longer maps onto one comparison.
- A caller needs a probabilistic finality signal (economic finality
  estimate) distinct from both the depth milestone and the settled bit.
- The reorg-downgrade sources need a third path (for example a push signal
  distinct from `ChainReorged`), changing the `{source}` label set.

## Alternatives Considered

### One terminal `confirmed` status at N confirmations

Rejected. Collapses the milestone and the guarantee into one fallible flag.
Either it fires before immutability (unsafe) or it waits for a conservative
depth that a UX would rather not (slow), and it strands SSE subscribers when
a "confirmed" block later reorgs out.

### Compute immutability from a depth heuristic instead of the settled tip

Rejected. Duplicates the chain plane's reorg-window accounting inside zpay
and drifts from it. The settled tip is the authoritative ceiling; a depth
count only approximates it.

### Single downgrade source (subscription only)

Rejected. A cursor expiry or a restart opens a window the subscription
misses. The poll plus the startup-and-cursor-expiry sweep is the completeness
backstop; dropping it would leave silent stale-mined rows after any
subscription gap.

## Out of Scope

- Probabilistic or economic finality estimates. This ADR covers the depth
  milestone and the settled-tip guarantee only.
- The chain plane's reorg-window derivation. Owned by zinder (ADR-0003).
- Per-merchant webhook delivery of downgrade events. The wire surface is the
  SSE snapshot and the pull status; a signed webhook is a separate decision.
