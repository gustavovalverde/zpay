# Railway deploy: zpay-runtime

Operator reference for deploying `zpay-runtime` to Railway alongside
the zinder chain plane. The service ships as a Dockerfile build, runs
behind a Railway-managed public domain, reaches zinder over private
networking, and persists libSQL state to a mounted volume.

## Service shape

| Field | Value |
| --- | --- |
| Railway service name | `zpay` |
| Project | Same project as `zinder` |
| Image source | `Dockerfile` at repo root |
| Public listener (app) | TCP 8080 |
| Ops listener | TCP 9295 (private) |
| Healthcheck | `GET /healthz` (200 with `{"status":"alive"}` when the listener is up) |
| Volume | `zpay-data` mounted at `/var/lib/zpay` |
| Initial public domain | `zpay.up.railway.app` (Railway-managed) |
| Custom domain (later) | `pay.zentity.xyz` |

## Required env vars

Set these from the Railway dashboard or via `railway variables set`.
The container refuses to start if any required value is missing or
invalid (fail-loud per ADR-0007).

| Variable | Required? | Example | Notes |
| --- | --- | --- | --- |
| `ZPAY_NETWORK` | yes | `testnet` | `mainnet` \| `testnet` \| `regtest` |
| `ZPAY_VERIFY__NETWORK` | yes | `testnet` | `mainnet` \| `testnet`; pins ZIP-311 BLAKE2b digest personalization. No default. |
| `ZPAY_EXPECTED_HOST` | yes | `zpay.up.railway.app` | DPoP `htu` host pin. Flip to `pay.zentity.xyz` once DNS lands. |
| `ZPAY_EXPECTED_SCHEME` | yes (prod) | `https` | Defaults to `https` when `ZPAY_EXPECTED_HOST` is set. |
| `ZPAY_STORE__BACKEND` | yes | `libsql` | Only `libsql` and `memory` recognised. |
| `ZPAY_STORE__URL` | yes | `file:/var/lib/zpay/zpay.libsql` | Use a `file:` URL for the mounted volume; `libsql://…` for Turso. |
| `ZPAY_PAYEES__CONFIG_PATH` | yes | `/etc/zpay/payees.toml` | The image bakes a placeholder file here; production overrides via bind-mount or custom image. |
| `ZPAY_CHAIN_SOURCE_URL` | yes (settle) | `http://zinder.railway.internal:9067` | Internal DNS provided by Railway. Without it, `/settle` returns 502. |
| `ZPAY_EXPLORER_URL` | yes (verify) | `http://zinder.railway.internal:9067` | Same shape. Without it, `/verify` reports `chain_presence: oracle_unavailable`. |
| `ZPAY_FINALITY_DEPTH` | optional | `3` | Default 3; bump for mainnet. |
| `ZPAY_STORE__AUTH_TOKEN` | optional | `<turso-token>` | Turso only. |
| `ZPAY_STATIC_TIP_FALLBACK` | optional | `4000000` | Only meaningful without a tip oracle. |
| `ZPAY_ALLOW_DEMO_PAYEE` | DEV ONLY | unset | Truthy values (`1`, `true`, `yes`) bypass the placeholder-receiver boot gate. Never set in production. |

## Volume provisioning

Provision the volume once per environment:

1. Railway dashboard → `zpay` service → Settings → Volumes → New.
2. Name: `zpay-data`. Mount path: `/var/lib/zpay`.
3. Restart the service so the volume mounts before the next boot.

The libSQL prepared-tx and settlement-ledger rows live in
`/var/lib/zpay/zpay.libsql`. Without a volume, the service starts but
loses state on every redeploy.

## Cross-service wiring (zinder)

zpay reaches the zinder chain plane over Railway private networking.
The hostname is `zinder.railway.internal` (the service name is the
DNS label).

Set both endpoints to the same gRPC port unless zinder exposes the
explorer plane on a different port:

```
ZPAY_CHAIN_SOURCE_URL=http://zinder.railway.internal:9067
ZPAY_EXPLORER_URL=http://zinder.railway.internal:9067
```

Private networking carries plaintext gRPC; there is no TLS termination
between services in the same Railway project.

## Domain provisioning

Two stages:

1. **Initial bring-up.** Generate a Railway-managed domain
   (`zpay.up.railway.app`) and pin it with `ZPAY_EXPECTED_HOST`. The
   service is reachable immediately; DPoP `htu` canonicalization binds
   to that exact host.
2. **Custom domain cutover.** Point `pay.zentity.xyz` (CNAME) at the
   Railway-managed domain via the dashboard. Update
   `ZPAY_EXPECTED_HOST=pay.zentity.xyz` and redeploy. Until the env
   var flips, requests reaching the custom hostname will fail DPoP
   `htu` validation by design.

## First-deploy checklist

Walk this in order. Each step is independent and can be reverted.

1. Confirm the placeholder-payee gate is OFF (`ZPAY_ALLOW_DEMO_PAYEE`
   unset). The runtime refuses to start when the baked-in
   `aether-demo` placeholder receiver is still active.
2. Stage the production `payees.toml`. Either bake it into a custom
   image at `/etc/zpay/payees.toml`, or bind-mount it via a Railway
   volume.
3. Provision the `zpay-data` volume at `/var/lib/zpay` (see above).
4. Set every required env var in the table above.
5. Run `./scripts/deploy-to-railway.sh --check` once to validate the
   stage tree and the `railway.toml` schema without pushing.
6. Run `./scripts/deploy-to-railway.sh` (or `--detach`) to push.
7. After the deploy completes, hit `https://zpay.up.railway.app/healthz`
   to confirm liveness, then `https://zpay.up.railway.app/x402/v2/accepts?payee_id=<id>`
   from outside Railway and confirm the listener responds.

## Rollback

Two paths, in order of speed:

- **Dashboard restore.** Railway tracks every deploy in the service
  history. Click the previous green deploy and "Restore". State on
  the `zpay-data` volume is unchanged; only the binary rolls back.
- **Script with prior commit.** Check out the prior commit locally
  and re-run `./scripts/deploy-to-railway.sh`. The script tags the
  deploy with `git rev-parse --short HEAD` so the dashboard history
  stays readable.

Avoid `--force` redeploys of a known-broken image; let the previous
deploy keep serving until the new image is staged.

## Healthcheck shape

Railway probes `GET /healthz` every 30 seconds. The endpoint:

- Returns **200** with body `{"status":"alive"}` for any healthy
  process. Used as "live".
- Returns **5xx** only on transport-level failure (binary crashed or
  the listener cannot accept connections).

`/healthz` is dependency-agnostic on purpose: it tells Railway that
the process answers, not that downstream services (zinder, the
payees file) are reachable. Use the operational logs and the WARN
posture lines below to spot dependency degradation; the healthcheck
itself is the platform-restart signal, not the on-call alarm.

The Dockerfile's container-level `HEALTHCHECK` uses the same path so
both layers agree.

## Operational warnings

The runtime emits structured `WARN` log lines for two posture issues
that operators must address before production traffic:

- `ZPAY_ALLOW_DEMO_PAYEE=1; running with placeholder payee <id>; do
  not use in production`. The placeholder-receiver gate has been
  bypassed. Acceptable only on docker-compose dev stacks; never in a
  Railway production environment.
- `ZPAY_EXPECTED_HOST unset; DPoP htu canonicalization uses inbound
  Host header. Set this in production.` Pin the host so an attacker
  cannot redirect DPoP-bound proofs through a different hostname.

Both warnings are intentional: the gate fails loud when it can refuse
to start safely, and warns when refusing to start would block dev.
