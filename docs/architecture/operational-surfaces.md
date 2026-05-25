# Operational Surfaces

This document is the operator's contract. It specifies the readiness state
machine, the ops listener shape, the env-var schema, the live-test gates,
and the diagnostics surface. Code is expected to match this doc; if they
diverge, this doc wins and the code is the bug.

## Process model

zpay is a single Rust binary, `zpay-runtime`. There is no sidecar, no
companion process, no init container. The binary runs an Axum HTTP server
on the main port, an ops listener on a separate port, and a libSQL
connection pool talking to a local or remote Turso database.

```text
zpay-runtime
   |
   +-- main listener  (HTTP, port from ZPAY_SERVER__BIND_ADDR)
   |     /x402/v2/*
   |     /mpp/v1/*       (feature-gated; off in v1)
   |     /openapi.json
   |
   +-- ops listener   (HTTP, port from ZPAY_OPS__BIND_ADDR)
   |     /healthz
   |     /readyz
   |     /metrics
   |
   +-- zally Wallet   (in-process; operator-owned seed)
   +-- libSQL conn    (zpay-store)
   +-- zinder client  (gRPC; from zinder-client::RemoteChainIndex)
```

The two listeners are deliberately separate. The main listener is public
(exposed by Railway / Cloudflare / whatever fronts the deployment); the
ops listener binds to a loopback or private network and is never exposed
externally. Metrics and readiness probes are not auth-gated; they are
private by deployment, not by code.

## Readiness state machine

`/readyz` returns a typed `Readiness` shape:

```json
{
  "status": "ready",
  "started_at_unix_seconds": 1748212812,
  "current_capabilities": ["x402.v2.accepts", "x402.v2.prepare", ...],
  "dependencies": {
    "zinder": { "reachable": true, "tip_height": 3217845, "derive_lag_blocks": 1 },
    "store": { "reachable": true, "schema_version": 1 },
    "compliance_jwks": { "reachable": true, "cached_at_unix_seconds": 1748212808 }
  }
}
```

States:

- `starting`: process is up; dependencies not yet probed. HTTP 503.
- `degraded`: at least one optional dependency unreachable (e.g.,
  compliance JWKS) but core capabilities work. HTTP 200, `status:
  degraded` and `current_capabilities` filtered to the working set.
- `ready`: all configured capabilities are live. HTTP 200, `status: ready`.
- `draining`: SIGTERM received; new requests rejected, in-flight
  requests allowed to complete up to the drain deadline. HTTP 503.
- `stopped`: process shut down. (Not observable; the listener is gone.)

Cause enum for `degraded`:

- `Reason::IndexerUnreachable`
- `Reason::StoreUnreachable`
- `Reason::SchemaMigrationPending`
- `Reason::ComplianceJwksUnreachable`
- `Reason::ChainStale`
- `Reason::CapabilityUnavailable { capability: String }`

## Ops listener endpoints

| Path | Method | Response | Purpose |
|------|--------|----------|---------|
| `/healthz` | GET | 200 with `{ status: "alive", started_at_unix_seconds }` | Liveness; never 503 unless the process is broken. |
| `/readyz` | GET | 200 or 503 with `Readiness` body | Dependency readiness. |
| `/startupz` | GET | 200 once startup completes, 503 before | Startup probe (k8s convention). |
| `/metrics` | GET | 200 with Prometheus text format | Metrics for scraping. |
| `/diagnostics` | GET | 200 with redacted config + capability status | Operator self-check; redacts all secrets. |

The diagnostics endpoint is the same shape as `--print-config` but
returns it as JSON over HTTP rather than to stdout.

## Configuration

zpay is configured by layering:

1. Code defaults.
2. The TOML config file (path from `--config`).
3. Environment variables matching `ZPAY_*` with `__` for nested fields.
4. CLI overrides (specific flags only; not all fields are flag-addressable).

Each later layer overrides the earlier ones. Production binaries strip
every key starting with `ZPAY_TEST_` from their env read.

### Required env vars (production)

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_NETWORK` | none | `mainnet`, `testnet`, or `regtest`. Required. |
| `ZPAY_SERVER__BIND_ADDR` | `127.0.0.1:8080` | Main HTTP listener. |
| `ZPAY_OPS__BIND_ADDR` | `127.0.0.1:9295` | Ops listener. |
| `ZPAY_NODE__INDEXER_GRPC_ADDR` | none | zinder query endpoint. Required for broadcast. |
| `ZPAY_WALLET__AGE_IDENTITY_TEXT` | none | Operator wallet age identity (sealed seed). Required to start. |
| `ZPAY_STORE__URL` | `file:./zpay.libsql` | libSQL connection URL. |
| `ZPAY_STORE__AUTH_TOKEN` | none | Turso auth token (required for remote URLs). |
| `ZPAY_COMPLIANCE__JWKS_URL` | `https://app.zentity.xyz/api/auth/oauth2/jwks` | PoH-token JWKS endpoint. |
| `ZPAY_COMPLIANCE__ACCEPTED_ISSUERS` | `["zentity"]` | Allowlist of `iss` claims. |
| `RUST_LOG` | `zpay=info` | tracing-subscriber filter. |

### Optional env vars

| Var | Default | Description |
|-----|---------|-------------|
| `ZPAY_SERVER__TLS__CERT_PATH` | none | TLS certificate path. Optional behind a load balancer. |
| `ZPAY_SERVER__TLS__KEY_PATH` | none | TLS key path. |
| `ZPAY_SERVER__CORS__ALLOWLIST` | `[]` | Origins allowed for browser clients. |
| `ZPAY_NODE__FALLBACK_EXPLORER_HTTP_ADDR` | none | zexplorer fallback for confirmation oracle. |
| `ZPAY_CACHE__DEFAULT_TTL_SECONDS` | `300` | Prepared-tx cache TTL. |
| `ZPAY_TELEMETRY__FORMAT` | `json` | `json` or `pretty`. |

### CLI subcommands

```text
zpay-runtime --help
zpay-runtime --config /etc/zpay/config.toml
zpay-runtime --print-config        # redacts every secret as [REDACTED]
zpay-runtime --describe-capabilities
zpay-runtime --check-config        # validates without starting the listener
```

## Secrets

zpay-owned processes receive secrets through canonical typed config
fields populated by `ZPAY_*` variables or the operator-provided TOML
layer. Paired `_path` companion env vars are banned for any secret:

- Operator wallet age identity: via `ZPAY_WALLET__AGE_IDENTITY_TEXT`,
  never `*__PATH`. The operator can paste the age identity into a
  Railway / Kubernetes secret directly.
- Turso auth token: via `ZPAY_STORE__AUTH_TOKEN`.
- Bearer-key allowlist hashes: stored in libSQL, derived from raw keys
  that are entered via the operator CLI (`zpay-ops bearer add`).

`--print-config`, `/diagnostics`, every log line, and every error
message redact secrets via `secrecy::Secret<T>` and a single
`redact_for_humans()` function. No secret ever appears in stdout, in
the Prometheus text format, or in a structured tracing field.

## Live-test gates

T3 tests are double-gated:

1. Compile-time: `#[ignore = LIVE_TEST_IGNORE_REASON]` on every T3 test.
2. Runtime: `zpay_testkit::live::require_live()` reads `ZPAY_TEST_LIVE`.

Mainnet T3 tests additionally require `ZPAY_TEST_ALLOW_MAINNET=1`.
Production binaries do not link `zpay-testkit`; the gates exist as a
defensive layer in case a test accidentally lands in a non-test binary.

## Tracing

`tracing-subscriber` with JSON output in production. Span fields that
must appear on every relevant span:

- `network` (always)
- `merchant_id` (when present)
- `payment_id` (when present)
- `txid` (when present)
- `dpop_jkt` (when present)
- `capability` (on capability-gated routes)
- `agent_id` (when an `agent_assertion` is presented)

PoH token JTI, raw bearer tokens, age identities, and wallet seed bytes
are forbidden as span fields.

## Shutdown

SIGTERM and SIGINT both trigger graceful shutdown:

1. Listener stops accepting new connections.
2. In-flight requests have `ZPAY_SERVER__DRAIN_TIMEOUT_SECONDS` (default
   30) to complete.
3. libSQL connections flush their write buffer.
4. The zinder subscription stream is closed; in-flight settlements
   complete before the process exits.

The `cancel_on_terminating_signal(CancellationToken)` helper from
`zpay-runtime` drives this; it mirrors zinder's shared pattern.

## Operational invariants

- One process per deployment unit. zpay does not coordinate across
  instances; horizontal scaling requires sticky routing on `payment_id`
  to avoid cache misses.
- Cache misses are not fatal: a missing `payment_id` returns
  `PaymentNotFound`, and the agent re-prepares. Idempotency keys
  protect against double-spend on the retry.
- libSQL is the only persistent state. Losing the database loses the
  prepared-tx cache (recoverable: agents re-prepare) and the settlement
  ledger (unrecoverable: requires reconciliation against zinder).
  Operators back up libSQL.
