# Proposal-0004: zexplorer per-txid watch with webhook delivery

| Field | Value |
| ----- | ----- |
| Status | Superseded |
| Consumer | zpay (fallback confirmation oracle) |
| Upstream | zexplorer |
| Pinned at | n/a (HTTP-only dependency) |
| Related | [PRD-42 Phase 2](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [ADR-0003](../adrs/0003-zinder-as-chain-plane.md) |

Superseded: zpay reaches zinder directly for the confirmation path in every
supported deployment, so the zexplorer fallback confirmation oracle is not
adopted (see [ADR-0003](../adrs/0003-zinder-as-chain-plane.md)). This ask is
retained for history only.

## Context

zexplorer wraps zinder's `ChainEvents` and `MempoolEvents` streams for its WebSocket pub/sub (`zexplorer/apps/api/src/routes/stream.ts`), but exposes no per-txid push surface. A consumer that wants notification when a specific txid reaches N confirmations must:

1. Subscribe to the global WebSocket `mempool.mined_v1` topic and filter client-side, or
2. Poll `GET /api/v1/{network}/transactions/{txid}` on a fixed cadence.

For zpay's fallback confirmation oracle (used when local zinder is unreachable), neither path fits: the WebSocket requires a long-lived connection from a stateless HTTP service, and polling burns calls.

## Ask

Add a per-txid watch route:

```http
POST /api/v1/{network}/transactions/{txid}/watch
Content-Type: application/json

{
  "callback_url": "https://example.com/zpay/watch-callback",
  "target_confirmations_count": 1,
  "expires_at_unix_seconds": 1748213000,
  "hmac_secret_b64": "..."
}
```

Response:

```json
{
  "watch_id": "01HZ...",
  "expires_at_unix_seconds": 1748213000
}
```

Backend behaviour:

- Subscribe to the txid via zinder's `ChainEvents` filter (already used by stream.ts).
- Persist watch state in Redis with the `expires_at_unix_seconds` TTL.
- On confirmation: POST the JSON payload `{ watch_id, txid, confirmations_count, block_height, observed_at_unix_seconds }` to the callback URL, signed with `X-Zexplorer-Signature: hmac-sha256=<hex>`.
- Fire-once semantics: deliver one callback, then expire. zpay re-registers on retry.

## Why this lives in zexplorer, not zpay

zexplorer already owns the Redis pub/sub fan-out for the public stream. Building a parallel watch service in zpay would duplicate that infrastructure for a single consumer.

Multiple non-zpay consumers benefit: a wallet that wants to notify a user when a payment confirms can use the same endpoint; a DeFi pool watching for liquidations can register watches at scale.

## Compatibility

Additive. Existing zexplorer routes unchanged. The route is unauthenticated in v1 (Redis-backed rate limiting only); a future ADR addresses bearer-key auth if abuse appears.

## Acceptance

- `POST /api/v1/{network}/transactions/{txid}/watch` returns 200 with a typed `watch_id`.
- Confirmation callbacks deliver within the configured TTL when zinder reports the target confirmation count.
- HMAC signature verifies against `X-Zexplorer-Signature` header.
- A Playwright E2E test asserts the full register-broadcast-confirm-callback flow against regtest.

This ask is superseded (see the header): zpay's confirmation oracle reads
zinder directly and adds no zexplorer fallback path.
