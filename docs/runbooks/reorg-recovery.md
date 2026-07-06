# Reorg recovery: zpay settlement

Operator reference for how zpay handles Zcash chain reorganizations, what it
does on its own, and the few cases that need an operator. The settlement
lifecycle and its guarantees are specified in
[ADR-0009](../adrs/0009-settlement-lifecycle-and-finality.md); this runbook is
the operational view.

## What the system does on its own

zpay treats a reorg as a normal, recoverable event. No operator action is
required for an ordinary reorg inside zinder's tracked window.

### Event-driven downgrades

The chain-event subscription tails zinder's `ChainEvents`. On a `ChainReorged`
envelope it reads the reverted block range and downgrades every settlement
row whose mined height falls inside it: the mined height clears, the
confirmation count zeroes, `reorg_count` increments, and `last_reorged_at`
stamps. Each downgrade increments
`zpay_reorg_downgrades_total{source="chain_event"}`.

### Poll reconciliation

A 60-second confirmation poll backstops the subscription. When it re-checks a
row that was mined and the chain plane now reports the transaction as
not-found, on a conflicting chain, or back in the mempool, it downgrades the
same way and increments `zpay_reorg_downgrades_total{source="poll"}`. The
subscription also runs a full reconciliation sweep on startup and after any
resume-cursor expiry, so a reorg that happened while zpay was disconnected is
caught on reconnect.

### SSE corrections

An open `GET /x402/v2/payments/{payment_id}/events` stream receives a fresh
snapshot whenever a downgrade touches its payment: the status returns to
`broadcast` (or `expired` if the expiry height has since lapsed) and
`reorg_count` rises. The stream does not close on `final`, so a subscriber
that saw `final` still receives the later downgrade.

### The settled gate

A payment at or below zinder's settled tip is immutable: `settled` is true and
no reorg can move it. The reconciliation poll skips rows already at or below
the settled tip, so a settled payment is never re-polled or downgraded. This
is the bit a caller keys an irreversible action on, not `final`.

## What the operator watches

| Signal | Where | Reading |
|--------|-------|---------|
| `reorg_count` | `GET /x402/v2/payments/{id}`, SSE snapshot | Per-payment regressions. A handful is normal chain behavior. |
| `zpay_reorg_downgrades_total{source}` | `/metrics` | Fleet-wide downgrade rate, split `poll` vs `chain_event`. A sustained spike signals a deep or repeated reorg. |
| `zpay_chain_visible_tip_height`, `zpay_chain_settled_tip_height` | `/metrics` | The gap between them is the live reorg window. |
| `zpay_chain_status_cache_age_seconds` | `/metrics`, `/readyz` | A climbing value means the poll loop or subscription stalled; downgrades may be delayed. |

An elevated downgrade rate with a healthy `zpay_chain_status_cache_age_seconds`
is zpay correctly tracking a noisy chain; no action beyond noting it. A
climbing cache age is the case to investigate: the chain plane may be
unreachable, and `/readyz` will read `not_ready` on the chain dependency.

## When zinder's reorg window is exceeded

If a reorg is deeper than zinder's tracked reorg window, zinder cannot emit a
bounded reverted range, and its store may be inconsistent. This is a
chain-plane recovery, not a zpay one:

1. Follow zinder's own store-reset runbook: wipe the zinder store and resync
   against Zebra. A store written below artifact schema 12 must be wiped and
   resynced regardless (see
   [upstream-platform-binding.md](../architecture/upstream-platform-binding.md)).
2. zpay needs no reset. Once zinder is resynced and reachable, the startup
   reconciliation sweep and the 60-second poll re-derive every unsettled row's
   status from the fresh chain view. Payments already `settled` are untouched.

## Payment-level reconciliation

Query the settlement ledger directly (the libSQL file at `ZPAY_STORE__URL`)
when investigating a specific payment or auditing regressions.

Payments that have regressed at least once:

```sql
SELECT payment_id, reorg_count, last_reorged_at, mined_block_height,
       confirmation_count
FROM settlement_ledger
WHERE reorg_count > 0
ORDER BY last_reorged_at DESC;
```

Success-kind rows still awaiting a mined block (candidates for a downgrade or
an expiry lapse):

```sql
SELECT payment_id, transaction_id, mined_block_height, confirmation_count,
       expiry_height
FROM settlement_ledger
WHERE broadcast_outcome_kind IN ('accepted', 'duplicate')
  AND mined_block_height IS NULL;
```

Cross-check a single payment against the live status projection with
`GET /x402/v2/payments/{payment_id}`: the `settled` flag and `reorg_count`
there reflect the current chain view, which the raw ledger row does not carry.
