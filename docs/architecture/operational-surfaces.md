# Operational Surfaces

This document is the operator's contract: the process model, the two
listeners, the readiness probe, the metrics, the env-var schema, and the
shutdown behavior. It tracks the shipped code; where the two diverge, the
code is authoritative and this document is the bug.

## Process model

zpay is a single Rust binary, `zpay-runtime`. There is no sidecar, no
companion process, no init container. The binary runs an Axum HTTP server on
the main port, an ops listener on a separate port, and a libSQL connection
to a local file or a remote Turso database. The wallet that signs agent
payments is a separate binary (`zspend-runtime`); zpay holds no spending key
and broadcasts only.

```text
zpay-runtime
   |
   +-- main listener  (HTTP, ZPAY_SERVER__BIND_ADDR, default 127.0.0.1:8080)
   |     /healthz
   |     /x402/v2/supported, /verify, /settle
   |     /zpay/v1/accepts, /tip, /prepare, /settle, /verify
   |     /zpay/v1/payments/{payment_id}
   |     /zpay/v1/payments/{payment_id}/events   (SSE)
   |
   +-- ops listener   (HTTP, ZPAY_OPS__BIND_ADDR, default 127.0.0.1:9295)
   |     /healthz
   |     /readyz
   |     /metrics
   |
   +-- libSQL conn    (zpay-store; prepared_tx + settlement_ledger)
   +-- zinder client  (gRPC; broadcast, ChainEvents, tip, disclosure fetch)
```

The two listeners are deliberately separate. The main listener is public
(fronted by Railway, Cloudflare, or whatever terminates TLS); the ops
listener binds to loopback or a private network and is never exposed
externally. `/metrics` and `/readyz` are not auth-gated; they are private by
deployment, not by code.

## Readiness

`GET /readyz` on the ops listener probes the chain plane and the store, then
returns HTTP 200 with `status: ready` or HTTP 503 with `status: not_ready`.
The body:

```json
{
  "status": "ready",
  "dependencies": {
    "chain": {
      "status": "ready",
      "live_probe": "ok",
      "visible_tip_height": 3217845,
      "settled_tip_height": 3217842,
      "cache_age_seconds": 4
    },
    "store": { "status": "ready", "probe": "ok" }
  },
  "listeners": { "app": "127.0.0.1:8080", "ops": "127.0.0.1:9295" }
}
```

Evaluation:

- **store.** A liveness read against the prepared-tx store. On failure,
  `store.status` is `not_ready`, `store.probe` carries the error string, and
  the overall status is `not_ready`.
- **chain.** With a chain plane configured (`ZPAY_CHAIN_SOURCE_URL` set), a
  live tip read runs under a 2-second timeout: `live_probe` is `ok`,
  `unreachable`, or `timeout`. The chain reads `ready` only when the live
  probe is `ok` **and** the shared chain-status cache is fresh, meaning
  `cache_age_seconds` is at or under 180 (three 60-second poll intervals).
  With no chain plane configured, `chain.status` and `live_probe` are
  `not_configured` and the chain does not gate readiness.

`visible_tip_height`, `settled_tip_height`, and `cache_age_seconds` come from
the shared chain view the confirmation poll and the chain-event subscription
refresh; they are `null` before the first chain read. A dead poll loop
surfaces as a growing `cache_age_seconds` even while the live probe still
succeeds, which is the signal an operator alerts on.

`GET /readyz` on `zspend-runtime` reports the wallet signer dependencies. The
probe returns HTTP 200 only when the issuer JWKS is loaded, revocation is fresh
or disabled in dev, and the wallet sync snapshot is fresh:

```json
{
  "network": "testnet",
  "sealed_seed": "dev",
  "posture": "dev",
  "jwks_cache": "loaded",
  "revocation_cache": "disabled",
  "wallet_sync": {
    "network": "testnet",
    "phase": "waiting",
    "sync_status": "at_tip",
    "scanned_height": 4152766,
    "safe_chain_tip_height": 4152766,
    "lag_blocks": 0,
    "snapshot_age_seconds": 2,
    "freshness": "fresh",
    "is_fresh": true,
    "last_fault": null
  }
}
```

`wallet_sync.freshness` is `fresh` only when the zally `SyncDriver` phase is
`syncing` or `waiting`, the snapshot network matches the signer network, both
heights are known, `lag_blocks` is within `ZSPEND_WALLET_SYNC_MAX_LAG_BLOCKS`,
and `snapshot_age_seconds` is within
`ZSPEND_WALLET_SYNC_STALE_AFTER_SECONDS`. A stale, recovering, parked, closing,
or closed sync driver makes `/readyz` return 503 and makes
`POST /v1/payments/sign` return retryable `wallet_unavailable` before the
access-token `jti` is reserved.

## Ops listener endpoints

| Path | Method | Response | Purpose |
|------|--------|----------|---------|
| `/healthz` | GET | 200, `{"status":"alive"}` | Liveness; answers whenever the process runs. |
| `/readyz` | GET | 200 or 503 with the JSON above | Dependency readiness. |
| `/metrics` | GET | 200, Prometheus text (`text/plain; version=0.0.4; charset=utf-8`) | Metrics for scraping. |
| `/payments` | GET | 200, JSON payments list + rate-limit snapshot | Operator payments console (see [ADR-0014](../adrs/0014-operator-payments-console.md)); accepts `?limit=` (default 50, max 200) and `?payee_id=`. |

`/healthz` is also mounted on the main listener so a platform health probe
(Railway, Kubernetes, an ALB) that reaches only the public port gets a 200
from a healthy process.

`/payments` is the first ops-listener route that carries payment content, not
only aggregate health data. The ops listener's existing "private by
deployment, not by code" contract (never expose it publicly) now protects
payment confidentiality as well as liveness data.

## Metrics

The Prometheus recorder is process-global; `/metrics` renders it. Counters
carry bounded label sets so cardinality stays fixed.

| Metric | Type | Labels |
|--------|------|--------|
| `zpay_requests_total` | counter | `route`, `outcome` (`success`, `client_error`, `server_error`, `other`) |
| `zpay_broadcast_outcomes_total` | counter | `kind` (`accepted`, `duplicate`, `invalid_encoding`, `rejected`, `unknown`) |
| `zpay_confirmation_updates_total` | counter | `outcome` (`mined`, `in_mempool`, `not_found`, `conflicting_chain`, `other`) |
| `zpay_reorg_downgrades_total` | counter | `source` (`poll`, `chain_event`) |
| `zpay_chain_reorgs_observed_total` | counter | none |
| `zpay_sse_subscribers` | gauge | none |
| `zpay_chain_visible_tip_height` | gauge | none |
| `zpay_chain_settled_tip_height` | gauge | none |
| `zpay_chain_status_cache_age_seconds` | gauge | none |
| `zspend_wallet_sync_snapshot_age_seconds` | gauge | none |
| `zspend_wallet_sync_fresh` | gauge | none |
| `zspend_wallet_sync_lag_blocks` | gauge | none |
| `zspend_wallet_sync_scanned_height` | gauge | none |
| `zspend_wallet_sync_safe_chain_tip_height` | gauge | none |

The chain gauges resample every 15 seconds from the shared chain view,
independent of the confirmation poll, so a stalled poll loop still shows a
climbing `zpay_chain_status_cache_age_seconds`.

`zpay_chain_reorgs_observed_total` increments on every `ChainReorged`
envelope the chain-event subscription receives, before the ledger downgrade
attempt. `zpay_reorg_downgrades_total{source="chain_event"}` only increments
when that downgrade actually returns rows, so a reorg that reverts a range
with no mined payments in it is still visible on the former counter even
though the latter stays flat.

## Configuration

zpay reads configuration from `ZPAY_*` environment variables, with `__` as
the nested-field separator. There is no TOML config layer for the runtime
itself; the only file input is the payee registry (`ZPAY_PAYEES__CONFIG_PATH`).

### Required

| Var | Description |
|-----|-------------|
| `ZPAY_VERIFY__NETWORK` | `mainnet` or `testnet`. No default; an unset or blank value fails startup with `VerifyNetworkMissing`. Pins the SLIP-44 coin type personalizing the ZIP-311 `BLAKE2b` digest the local verifier reconstructs. Regtest deployments pin to `testnet` (regtest carries no distinct SLIP-44 number). See [ADR-0007](../adrs/0007-local-zip311-verifier.md). |

### Networking and stores

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_SERVER__BIND_ADDR` | `127.0.0.1:8080` | Main HTTP listener. Invalid value fails startup. |
| `ZPAY_OPS__BIND_ADDR` | `127.0.0.1:9295` | Ops listener. Invalid value fails startup. |
| `ZPAY_NETWORK` | `regtest` | `mainnet`, `testnet`, or `regtest`. |
| `ZPAY_CHAIN_SOURCE_URL` | none | zinder gRPC endpoint. Unset disables broadcast (`/settle` returns 502) and settlement reconciliation. |
| `ZPAY_EXPLORER_URL` | none | zinder explorer-plane gRPC endpoint for ZIP-311 disclosure fetch. Unset makes `/verify` report `chain_presence: oracle_unavailable`. |
| `ZPAY_PAYEES__CONFIG_PATH` | none | TOML payee registry. Unset starts with an empty registry. A read or parse failure fails startup. |
| `ZPAY_STORE__BACKEND` | `libsql` | `libsql` or `memory`. Any other value fails startup. |
| `ZPAY_STORE__URL` | `file:./zpay.libsql` | libSQL connection URL. `file:<path>` for local SQLite, `libsql://<host>` for Turso. |
| `ZPAY_STORE__AUTH_TOKEN` | none | Turso auth token (remote URLs only). |
| `ZPAY_STATIC_TIP_FALLBACK` | `4000000` | Chain-tip fallback used only when no chain plane is configured. Invalid value fails startup. |
| `ZPAY_FINALITY_DEPTH` | `3` | Confirmation count at which `Mined` becomes `Final`. Raise for mainnet. Invalid value fails startup. |

### DPoP host pinning

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_EXPECTED_HOST` | none | Host the DPoP `htu` canonicalization pins against. Unset emits a startup `WARN` and falls back to the inbound `Host` header. |
| `ZPAY_EXPECTED_SCHEME` | `https` when a host is pinned, else `http` | Scheme the DPoP verifier expects. |

### Rate limiting and CORS

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE` | `120` | Per-DPoP-`jkt` budget per fixed 60-second window on the authenticated routes. `0` disables this dimension. A present-but-unparseable value fails startup. |
| `ZPAY_RATE_LIMIT__PER_IP_PER_MINUTE` | `600` | Per-client-IP budget per fixed 60-second window on the unauthenticated routes. `0` disables. Unparseable value fails startup. |
| `ZPAY_RATE_LIMIT__TRUST_FORWARDED_HEADERS` | off | Truthy (`1`, `true`, `yes`) lets the per-IP dimension key on `X-Forwarded-For`/`X-Real-IP`. Only enable behind a reverse proxy that terminates every inbound connection and sets the header itself; a direct caller controls those headers otherwise and can bypass the limiter. |
| `ZPAY_SERVER__CORS__ALLOWLIST` | none | Comma-separated exact origins allowed for browser clients. Empty or unset emits no CORS headers, so cross-origin browser calls stay blocked. |

The limiter is an in-memory fixed-window counter keyed by `jkt` or client IP.
The client IP is read from the peer socket by default. Only when
`ZPAY_RATE_LIMIT__TRUST_FORWARDED_HEADERS` is enabled does it prefer
`X-Forwarded-For` (leftmost hop) or `X-Real-IP`, falling back to the peer
socket when neither header is present. A limited request returns HTTP 429
with a `Retry-After` header and the standard problem envelope (see
[error-vocabulary.md](../reference/error-vocabulary.md)).

The official x402 routes are unauthenticated facilitator routes. They are rate
limited by client IP, advertise the configured Zcash `exact` payment kind, and
settle `x402-zcash-exact-v1` authorizations by verifying, extracting, and
broadcasting `pczt-v2-extractable` PCZT bytes. The zpay lifecycle routes keep
their DPoP-bound prepare and settle contract. Because verification and
settlement both run PCZT extraction, the runtime must have Sapling verifying
parameters available at the `zcash_proofs` default location. The Docker image
sets `HOME=/opt/zpay-home`; compose mounts
`${ZCASH_PARAMS_HOST_DIR:-${HOME}/.local/share/ZcashParams}` at both
`/opt/zpay-home/.zcash-params` and `/home/zpay/.zcash-params`.

### Dev-only

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_ALLOW_DEMO_PAYEE` | off | Truthy (`1`, `true`, `yes`) bypasses the placeholder-receiver boot gate for dev and compose stacks. Emits a `WARN` per offending payee. Never set in production. |
| `RUST_LOG` | `zpay=info` | `tracing-subscriber` env filter. |

### Demo UI gateway

`zpay-demo` is a separate dev-only binary for browser demonstrations. It is not
part of the production zpay process model. The gateway binds to loopback by
default, opens a local testnet wallet, owns demo DPoP and issuer keys, and
calls the existing `zpay-runtime`, `zspend-runtime`, zinder, fauzec, and
zexplorer surfaces.

Routes:

| Path | Method | Purpose |
|------|--------|---------|
| `/demo/v1/readiness` | GET | Readiness projection for zpay, zspend, zinder, wallet, faucet, and network. |
| `/demo/v1/wallet` | GET | Demo wallet address, balances, funding posture, and network. |
| `/demo/v1/faucet-claims` | POST | Submit a fauzec claim for the demo wallet. |
| `/demo/v1/faucet-claims/{request_id}` | GET | Poll a fauzec claim. |
| `/demo/v1/payments` | GET | List payments made this session, most recent first. |
| `/demo/v1/payments` | POST | Prepare a checkout through zpay. |
| `/demo/v1/payments/{payment_id}/settle` | POST | Sign and settle using the stored payment mode. |
| `/demo/v1/payments/{payment_id}` | GET | Enriched payment status for the UI. |
| `/demo/v1/payments/{payment_id}/events` | GET | SSE status stream for the UI. |
| `/demo/v1/payments/{payment_id}/verify` | POST | Verify the wallet-produced disclosure using server-held payment expectations. |
| `/demo/v1/console/payments` | GET | Proxy to zpay's ops-listener `GET /payments` operator console. |

Configuration:

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_DEMO_BIND_ADDR` | `127.0.0.1:7410` | Demo gateway listener. Non-loopback binds are for deliberate local demos only. |
| `ZPAY_DEMO_NETWORK` | `testnet` | `testnet` or `regtest`. `mainnet` is refused. |
| `ZPAY_DEMO_ZPAY_URL` | `http://127.0.0.1:8080` | zpay main listener. |
| `ZPAY_DEMO_ZPAY_OPS_URL` | `http://127.0.0.1:9295` | zpay ops listener for readiness. |
| `ZPAY_DEMO_ZSPEND_URL` | `http://127.0.0.1:8090` | zspend listener used by the gateway. |
| `ZPAY_DEMO_ZSPEND_PUBLIC_URL` | `ZPAY_DEMO_ZSPEND_URL` | URL encoded into zspend DPoP proofs. |
| `ZPAY_DEMO_ZINDER_URL` | `http://127.0.0.1:19101` | zinder gRPC endpoint for the demo wallet. |
| `ZPAY_DEMO_WALLET_DIR` | `.tmp/zpay-demo/wallet` | Local wallet seed and storage directory. |
| `ZPAY_DEMO_BIRTHDAY_HEIGHT` | zinder visible tip minus 500 blocks | Optional demo wallet birthday override. Leave unset for fresh demo wallets. |
| `ZPAY_DEMO_PAYEE_ID` | `aether-demo` | zpay payee used for prepare. |
| `ZPAY_DEMO_RESOURCE_URI` | `https://zpay.local/demo/reports/aether-brief` | Resource URI bound into prepare. |
| `ZPAY_DEMO_FAUZEC_URL` | `https://fauzec.com` | Faucet base URL. |
| `ZPAY_DEMO_ZEXPLORER_TX_URL` | `https://zexplorer.app/testnet/tx` | Explorer transaction URL prefix. |
| `ZPAY_DEMO_ISSUER_KEY_PATH` | `$ZPAY_DEMO_WALLET_DIR/dev-issuer-p256.pem` | Ed25519 or P-256 private key used to mint dev `payment_authorization` tokens for autopay. If absent, the gateway creates a local P-256 issuer key. |
| `ZPAY_DEMO_ISSUER_JWKS_PATH` | `$ZPAY_DEMO_WALLET_DIR/dev-jwks.json` | JWKS written when the gateway creates the default P-256 issuer key. Configure zspend with the matching `ZSPEND_JWKS_FILE`. |
| `ZPAY_DEMO_ISSUER_KID` | `zpay-demo-dev` | JWT `kid` for demo-issued autopay tokens. |
| `ZPAY_DEMO_ZSPEND_AUDIENCE` | `urn:zpay:zspend:local-dev` | Audience expected by zspend for demo-issued tokens. |
| `ZPAY_DEMO_TOKEN_TTL_SECONDS` | `120` | Demo access-token TTL. |
| `ZPAY_DEMO_MIN_FUNDED_ZAT` | `15000` | Minimum wallet balance before the UI can prepare a payment. |

### zspend-runtime wallet sync

`zspend-runtime` uses the `ZSPEND_*` namespace. The wallet opens or creates its
account at startup, then a long-lived zally `SyncDriver` keeps the wallet at
the configured zinder chain tip.

| Var | Default | Description |
|-----|---------|-------------|
| `ZSPEND_CHAIN_SOURCE_URL` | none | zinder gRPC endpoint. Required to materialise a fresh wallet account and to run continuous wallet sync. |
| `ZSPEND_BIRTHDAY_HEIGHT` | network default | Optional wallet birthday override. Use for repros such as Orchard divergence from height `4050200`. |
| `ZSPEND_WALLET_SYNC_POLL_INTERVAL_MS` | `5000` | Polling cadence when no chain event arrives. |
| `ZSPEND_WALLET_SYNC_MAX_ITERATIONS_PER_WAKE_COUNT` | `1000` | Maximum `Wallet::sync` iterations for one driver wakeup. |
| `ZSPEND_WALLET_SYNC_TIMEOUT_SECONDS` | `120` | Timeout for one wallet sync iteration. |
| `ZSPEND_WALLET_SYNC_MAX_LAG_BLOCKS` | `3` | Maximum signer lag accepted by `/readyz` and `/v1/payments/sign`. |
| `ZSPEND_WALLET_SYNC_STALE_AFTER_SECONDS` | `30` | Maximum age for the latest sync snapshot. |

## CLI

`zpay-runtime` takes a single flag and no subcommands:

```text
zpay-runtime                 # start both listeners
zpay-runtime --print-config  # log the resolved bind addresses and network
                             # with secrets redacted as [REDACTED], then exit
```

There is no `--describe-capabilities`, `--check-config`, `--config`,
`/startupz`, or `/diagnostics`. Startup validation happens by starting: every
required or invalid env value fails the boot with a typed `StartupError`.

## Secrets

zpay-runtime holds no wallet seed and no compliance credential. The only
secret it reads is the Turso auth token (`ZPAY_STORE__AUTH_TOKEN`).
`--print-config` redacts it as `[REDACTED]`; it never appears in the
Prometheus text or a tracing field.

## Tracing

`tracing-subscriber` with JSON output. The filter comes from `RUST_LOG`,
defaulting to `zpay=info`. The main router carries a `tower_http` trace layer.

## Shutdown

`SIGTERM` and `SIGINT` both trigger graceful shutdown: the listeners stop
accepting new connections and in-flight requests are allowed to complete
before the process exits. The background tasks (prepared-tx sweeper,
confirmation poll, chain-event subscription, chain-status sampler) are
detached and end with the process.

## Live validation

`zpay-e2e` is a standalone binary (not a test gate) that drives the full zpay
lifecycle against a running `zpay-runtime` plus zinder: `/zpay/v1/prepare`,
compose the protocol memo, propose and sign through a real zally wallet, POST
the signed bytes to `/zpay/v1/settle`, then poll
`/zpay/v1/payments/{payment_id}` until the
confirmation oracle observes the transaction mine. Funding is out-of-band
through fauzec; the harness prints a u-address and exits when the balance is
too low. The operator procedure lives in
[end-to-end-validation.md](../runbooks/end-to-end-validation.md).

## Operational invariants

- One process per deployment unit. zpay does not coordinate across instances;
  horizontal scaling requires sticky routing on `payment_id` to avoid cache
  misses, and the rate limiter is per-process.
- Cache misses are not fatal: a missing `payment_id` reports `never_issued`
  or an expired status, and the agent re-prepares. Idempotency keys protect
  against double-spend on the retry.
- libSQL is the only persistent state. Losing the database loses the
  prepared-tx cache (recoverable: agents re-prepare) and the settlement
  ledger (requires reconciliation against zinder). Operators back up libSQL.
