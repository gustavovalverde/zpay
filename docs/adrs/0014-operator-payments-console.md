# ADR-0014: Operator payments console boundary

| Field | Value |
| ----- | ----- |
| Status | Accepted |
| Product | zpay |
| Domain | Operator surface, settlement ledger schema |
| Related | [ADR-0004](0004-libsql-prepared-tx-cache.md), [ADR-0006](0006-facilitator-trust-boundary.md), [ADR-0009](0009-settlement-lifecycle-and-finality.md), [Operational surfaces](../architecture/operational-surfaces.md) |

## Context

An operator running zpay has no way to see the payments flowing through their
own deployment. `GET /readyz` answers "is the process healthy" (chain plane,
store); `GET /metrics` answers "what are the aggregate counters"; neither
answers "show me the last N payments and which payee they belong to." The gap
is not a missing convenience: without it, an operator debugging a stuck
payment or auditing payee activity has no query surface at all, only direct
database access.

Building this surface means answering two questions this repository has not
answered before: where does a cross-payee, content-bearing operator route
live, and does the data it needs even exist in the store today.

**Where it lives.** The main listener (`ZPAY_SERVER__BIND_ADDR`) is the public
x402 and zpay lifecycle surface, rate-limited and reachable from any client.
The ops listener (`ZPAY_OPS__BIND_ADDR`) already carries a different,
stricter posture: `operational-surfaces.md` documents it as "private by
deployment, not by code," meaning the operator is responsible for keeping it
off the public network, and the code adds no auth because none of its
existing routes (`/healthz`, `/readyz`, `/metrics`) carry anything
confidential. A payments list is the first ops-listener route that does.

**Whether the data exists.** It does not, fully. `prepared_tx` carries
`payee_id` and `amount_zat`, but `PreparedTxStore::remove` deletes that row
the instant a payment settles successfully, by design, to enforce fire-once
idempotency (`zpay-core/src/prepare.rs`). `settlement_ledger` is the
permanent record of every settle attempt, but its schema
(`crates/zpay-store/migrations/0001_initial.sql`,
`0002_reorg_aware_settlement.sql`) never carried `payee_id` or `amount_zat` at
all. So today, the moment a payment settles, its payee and amount association
is gone from persistent storage. A payee-attributed payments list needs a
schema change, not just a new read query.

## Decision

1. **New route on the ops listener, not the main listener.** `GET /payments`
   (unversioned, matching the sibling `/healthz` / `/readyz` / `/metrics`
   convention: the ops listener has never used a `/v1` prefix) lists recent
   settlement records, most recent first, optionally filtered by `payee_id`.
   It reuses the ops listener's existing trust model instead of introducing a
   bearer token, API key, or other new auth mechanism. The operational
   guidance in `operational-surfaces.md` that the ops listener must never be
   exposed publicly now protects payment confidentiality, not only liveness
   data; this ADR does not change that guidance, it raises the stakes on it.

2. **`settlement_ledger` gains `payee_id` and `amount_zat`.** Migration
   `0003_settlement_ledger_payee_and_amount.sql` adds both columns, `NOT
   NULL` for new rows. They are captured at settle time from the
   `prepared_tx` row already looked up before its deletion, so no new lookup
   is added to the settle path. `SettlementLedgerEntry` (`zpay-core::status`)
   gains the two fields; every call site of `SettlementLedgerStore::record`
   is updated to supply them. Existing rows written before this migration
   have no `payee_id`/`amount_zat` and are excluded from payee-filtered
   queries; this is an accepted gap for historical data, not a backfill this
   ADR takes on.

3. **List query is bounded, not paginated.** `payment_id` is a ULID
   (lexicographically time-sortable), so `ORDER BY payment_id DESC LIMIT N`
   gives "most recent" without a timestamp column or a cursor. A new index on
   `payee_id` supports the filtered case. No `OFFSET`-based pagination is
   added: the console shows a bounded recent window, not a full history
   browser. Revisit if a concrete operator workflow needs the latter.

4. **Dependency health reuses `/readyz`'s existing evaluation.** The console
   shows chain-plane and store status exactly as `/readyz` computes them
   today. It does not show `zspend`, `wallet`, or `faucet` status: those are
   `zpay-demo`'s dev-only readiness concepts, and zpay-runtime has no way to
   check them. A console that fabricated those rows would misrepresent what
   the production process actually knows.

5. **Chain-event and rate-limit stats reuse existing in-process state,
   not new metrics.** Chain tip and reorg counters already live in
   `ChainStatusCache` (an atomics-backed struct `/readyz` already reads
   directly, independent of the Prometheus recorder, which exposes no
   per-metric getter). The console reads the same cache. Rate-limit stats
   need one new accessor on `RateLimiter` returning current per-key window
   counts; the limiter's internal map has no such accessor today.

6. **New typed response structs, not the ad hoc `serde_json::json!` pattern
   `/readyz` uses.** This is the first typed response on the ops router;
   `/readyz`'s existing ad hoc body is left as-is rather than retrofitted in
   the same change.

## Rationale

Reusing the ops listener's trust boundary is the smallest change that
satisfies "operator-only, never public": it adds zero new authentication
code, at the cost of the operator being responsible for the listener's
network exposure, which is already the documented deployment contract for
every other ops route. Inventing a bearer-token scheme instead would add a
credential-issuance and rotation story this repository does not otherwise
have, for a route with the same confidentiality requirement existing routes
already assume the deployment enforces.

Adding `payee_id`/`amount_zat` to `settlement_ledger` at record time, rather
than reconstructing them from `prepared_tx` after the fact, respects the
fire-once deletion `prepare.rs` documents as a deliberate idempotency
control; this ADR does not touch that deletion.

## Consequences

Positive:

- An operator can see recent payment activity without direct database
  access.
- The schema gap (settled payments losing payee/amount association) is
  closed going forward.

Negative:

- `settlement_ledger`'s schema gains two columns and one more thing every
  future settle-path change must keep in sync.
- Every existing call site of `SettlementLedgerStore::record` changes
  signature in the same release.
- Rows written before this migration cannot be payee-filtered.

Neutral:

- No new configuration variable: the route is reachable wherever the ops
  listener already is.

## Switch Criteria

Revisit the "no pagination" decision if an operator workflow needs to browse
history beyond the bounded recent window. Revisit the "no new auth" decision
if zpay ever needs the console reachable from a network the operator does not
fully control (for example, a hosted-dashboard product); that is a
substantially different trust model and deserves its own ADR, not an
extension of this one.

## Alternatives Considered

### Mount the route on the main listener, gated by a new bearer token

Rejected. This is the first authentication mechanism in the zpay-runtime
binary outside DPoP, which is a payer-proof scheme, not an operator-credential
scheme. Standing up token issuance, storage, and rotation for one read route
is a disproportionate amount of new surface next to reusing a trust boundary
that already exists and already carries the exact "operator-only" contract
this route needs.

### Reconstruct payee attribution from `prepared_tx` without a schema change

Rejected. `prepared_tx` rows are deleted on successful settle by design
(fire-once idempotency). Any settled payment older than "still in-flight"
already has no `prepared_tx` row to read from; the data plainly is not there
without persisting it somewhere at record time.

### Expose the metrics recorder's counters directly for rate-limit stats

Rejected. `PrometheusHandle` (the installed recorder) exposes only text and
protobuf rendering, no per-metric current-value getter. Parsing the
recorder's own text output back into structured data is more code and more
fragile than adding one accessor to `RateLimiter`, which already holds the
counts in memory.
