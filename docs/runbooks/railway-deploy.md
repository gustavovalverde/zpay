# Railway deploy

Operator reference for deploying the zpay stack to Railway.

> **Deployment blocked.** Zinder-backed zpay is not currently a supported
> Railway deployment. The existing `zinder-v12` Railway target runs only
> `zinder-ingest`; it does not run the projector or native query service and
> therefore does not expose WalletQuery on port 9102. Changing a client port
> cannot supply that missing service. Resume this runbook only after a complete
> externally hosted Zinder wallet-serving topology (ingest, projector, native
> query, and compatibility service) has a reachable native WalletQuery URL.
Once the Zinder topology gate is cleared, one Railway project (`zcash-faucet`) hosts three
services built from this repository alongside the chain plane they consume:

```text
zebra  <--  external Zinder wallet-serving topology
                         ^       ^       ^
                         |       |       |
                       zpay <- zpay-demo -> zspend
          (8080, public)     (7410, public)   (8090, private)
```

After the deployment gates above are cleared, every deploy goes through
`scripts/deploy-to-railway.sh`. A raw
`railway up` from the repo root is unsupported: Railway always builds a
`Dockerfile` it finds at the upload root, so any service deployed that
way would get zpay's image.

## Service matrix

| | zpay | zspend | zpay-demo |
| --- | --- | --- | --- |
| Binary | `zpay-runtime` | `zspend-runtime` | `zpay-demo` |
| Dockerfile | `Dockerfile` | `Dockerfile.zspend` | `Dockerfile.zpay-demo` |
| Railway config | `railway.zpay.toml` | `railway.zspend.toml` | `railway.zpay-demo.toml` |
| App port / `PORT` | 8080 | 8090 | 7410 |
| Healthcheck | `/healthz` | `/healthz` | `/demo/v1/readiness` |
| Exposure | public domain | private network only | public domain |
| Volume | `/var/lib/zpay` | `/var/lib/zspend` | `/var/lib/zpay-demo` |

## Deploy command

```bash
./scripts/deploy-to-railway.sh <zpay|zspend|zpay-demo|all> --check   # stage + validate only
./scripts/deploy-to-railway.sh <zpay|zspend|zpay-demo|all>           # push
./scripts/deploy-to-railway.sh <service> --detach                    # push without log streaming
```

The script stages each service into its own temp directory holding an
allowlisted file set (the workspace manifests, `crates/`, `docker/`,
plus `etc/aether-demo.toml` for zpay and `demo/` for zpay-demo), then
overlays `Dockerfile.<service>` as `./Dockerfile` and
`railway.<service>.toml` as `./railway.toml`, validates the staged
config, and runs `railway up <stage> --path-as-root --service <name>`.
`all` deploys zspend, zpay, zpay-demo sequentially with fail-fast.

Two Railway platform behaviors force this shape:

- A `railway.toml` at the upload root applies to **any** service built
  from that tree, overriding dashboard and API settings. Each staged
  tree therefore carries exactly one `railway.toml` and one
  `Dockerfile`.
- Healthchecks probe the port carried in the `PORT` service variable,
  injecting an arbitrary value when unset. Each service needs `PORT`
  pinned to its app port (see the matrix); the script asserts this
  before pushing and prints the `railway variable set` remediation on
  mismatch.

## zpay required env vars

The container refuses to start if any required value is missing or
invalid (fail-loud per ADR-0007). Var semantics live in
[operational-surfaces.md](../architecture/operational-surfaces.md);
this table carries the deployment values.

| Variable | Required? | Value | Notes |
| --- | --- | --- | --- |
| `PORT` | yes | `8080` | Healthcheck probe port; the app itself binds via the image's `ZPAY_SERVER__BIND_ADDR`. |
| `ZPAY_NETWORK` | yes | `testnet` | `mainnet` \| `testnet` \| `regtest` |
| `ZPAY_EXPECTED_HOST` | yes | `zpay-production.up.railway.app` | DPoP `htu` host pin. Flip to `pay.zentity.xyz` once DNS lands. |
| `ZPAY_EXPECTED_SCHEME` | yes (prod) | `https` | Defaults to `https` when `ZPAY_EXPECTED_HOST` is set. |
| `ZPAY_STORE__BACKEND` | yes | `libsql` | Only `libsql` and `memory` recognised. |
| `ZPAY_STORE__URL` | yes | `file:/var/lib/zpay/zpay.libsql` | `file:` URL on the mounted volume; `libsql://…` for Turso. |
| `ZPAY_PAYEES_TOML` | yes (Railway) | TOML text | `docker/start.sh` writes it to `ZPAY_PAYEES__CONFIG_PATH` on every start. `etc/aether-demo.testnet.toml` is the tracked source of truth for the testnet value. |
| `ZPAY_CHAIN_SOURCE_URL` | yes | `<native-wallet-query-url>` | Must target the native query service of a complete externally hosted Zinder wallet-serving topology. The ingest-only `zinder-v12` Railway service is invalid. |
| `ZPAY_FINALITY_DEPTH` | optional | `3` | Default 3; bump for mainnet. |
| `ZPAY_STORE__AUTH_TOKEN` | optional | `<turso-token>` | Turso only. |
| `ZPAY_ALLOW_DEMO_PAYEE` | DEV ONLY | unset | Bypasses the placeholder-receiver boot gate. Never set in production. |

## zspend env vars

zspend stays private-network only; no public domain. The sealed seed at
`/var/lib/zspend/wallet.age` is the only wallet backup.

| Variable | Value | Notes |
| --- | --- | --- |
| `PORT` | `8090` | Healthcheck probe port. |
| `ZSPEND_NETWORK` | `testnet` | |
| `ZSPEND_PUBLIC_URL` | `http://zspend.railway.internal:8090` | Base of the sign URL the DPoP `htu` verification pins against; must match the caller's `ZPAY_DEMO_ZSPEND_PUBLIC_URL`. Unset emits a startup `WARN`. |
| `ZSPEND_CHAIN_SOURCE_URL` | `<native-wallet-query-url>` | Same externally hosted native WalletQuery service as zpay. |
| `ZSPEND_AUDIENCE` | deployment-chosen URN | Must match the issuer's audience claim. |
| `ZSPEND_JWKS_JSON` + `ZSPEND_JWKS_FILE` | JWKS text + `/var/lib/zspend/dev-jwks.json` | `docker/start-zspend.sh` materializes the JSON to the file path on every start. |
| `ZSPEND_BIRTHDAY_HEIGHT` | near-tip height | Set for fresh wallets so first-boot sync stays inside the healthcheck window. |
| `ZSPEND_ALLOW_AUTO_PROVISION`, `ZSPEND_ALLOW_DEV_SEED` | `1` (test only) | Dev-posture seed handling; production provisions the seed explicitly and fronts it with a KMS. |

## zpay-demo env vars

| Variable | Value | Notes |
| --- | --- | --- |
| `PORT` | `7410` | Healthcheck probe port. |
| `ZPAY_DEMO_NETWORK` | `testnet` | `mainnet` is refused. |
| `ZPAY_DEMO_ZPAY_URL` | `http://zpay.railway.internal:8080` | Call URL; traffic stays on private networking. |
| `ZPAY_DEMO_ZPAY_PUBLIC_URL` | `https://zpay-production.up.railway.app` | URL encoded into zpay DPoP proofs; must match zpay's `ZPAY_EXPECTED_HOST` and scheme. |
| `ZPAY_DEMO_ZPAY_OPS_URL` | `http://zpay.railway.internal:9295` | |
| `ZPAY_DEMO_ZSPEND_URL` | `http://zspend.railway.internal:8090` | `ZPAY_DEMO_ZSPEND_PUBLIC_URL` defaults to this and must match zspend's `ZSPEND_PUBLIC_URL`. |
| `ZPAY_DEMO_ZINDER_URL` | `<native-wallet-query-url>` | Same externally hosted native WalletQuery service as zpay and zspend. |
| `ZPAY_DEMO_PAYEE_ID` | `aether-demo` | |
| `ZPAY_DEMO_ISSUER_KEY_PEM` + `ZPAY_DEMO_ISSUER_KEY_PATH` | PEM text + `/var/lib/zpay-demo/wallet/dev-issuer-p256.pem` | `docker/start-zpay-demo.sh` materializes the PEM on every start. The matching JWKS is zspend's `ZSPEND_JWKS_JSON`. |
| `ZPAY_DEMO_ISSUER_KID` | key id | Must match the `kid` in zspend's JWKS. |
| `ZPAY_DEMO_ZSPEND_AUDIENCE` | same URN as `ZSPEND_AUDIENCE` | |

Do not set: `ZPAY_DEMO_STATIC_DIR` (the image bakes `/app/static`) and
`RAILWAY_DOCKERFILE_PATH` (inoperative under the staged-overlay
scheme).

## Volume provisioning

Provision one volume per service, once per environment, via the
dashboard (service → Settings → Volumes) or `railway volume`. Mount
paths are in the service matrix. Losing zpay's volume loses the
prepared-tx cache (recoverable) and the settlement ledger (requires
reconciliation); losing zspend's volume destroys the wallet seed unless
it was provisioned restorably.

## Domain provisioning

zpay and zpay-demo get Railway-managed domains (dashboard or
`railway domain`); zspend gets none. After generating zpay's domain,
set `ZPAY_EXPECTED_HOST` to the assigned hostname and redeploy; until
the var matches, requests fail DPoP `htu` validation by design. The
custom-domain cutover to `pay.zentity.xyz` follows the same pattern:
CNAME first, then flip `ZPAY_EXPECTED_HOST` and
`ZPAY_DEMO_ZPAY_PUBLIC_URL` together.

## First-deploy checklist

1. Confirm a complete externally hosted Zinder wallet-serving topology is
   ready and record its native WalletQuery URL. Do not use the ingest-only
   `zinder-v12` Railway service.
2. Set every variable in the tables above (including `PORT`).
3. Provision the volumes.
4. `./scripts/deploy-to-railway.sh all --check` to validate the staged
   trees offline.
5. `./scripts/deploy-to-railway.sh all` and watch the healthchecks.
6. `https://<zpay-domain>/healthz`, then
   `https://<zpay-domain>/zpay/v1/accepts?payee_id=<id>` from outside
   Railway.
7. `https://<zpay-demo-domain>/demo/v1/readiness` reports zpay, zspend,
   zinder, and faucet `ready`.

## Rollback

- **Dashboard restore.** Every deploy is in the service history;
  restore the previous green one. Volume state is unchanged.
- **Script with prior commit.** Check out the prior commit and re-run
  `./scripts/deploy-to-railway.sh <service>`. The deploy message
  carries `git rev-parse --short HEAD` (suffixed `+dirty` when the
  tree has uncommitted changes) so the history stays readable.

## Healthcheck shape

Railway probes each service's configured healthcheck path on its `PORT`
every few seconds during rollout, with a 5-minute window. `/healthz`
(zpay, zspend) is dependency-agnostic liveness. `/demo/v1/readiness`
(zpay-demo) always answers 200 once the process serves; dependency
status lives in the body, so it too acts as liveness for the platform.
The Dockerfiles' container-level `HEALTHCHECK` directives use the same
paths so both layers agree.

## Sapling parameters

`/x402/v2/verify` and `/x402/v2/settle` extract signed
`pczt-v2-extractable` bytes, which loads Sapling verifying parameters
through `zcash_proofs`. All three images bake `sapling-spend.params`
and `sapling-output.params` into the image at build time (fetched from
`download.z.cash`, the same source `fetch-params.sh` uses), so Railway
services need no volume or extra configuration for this. Local compose
stacks instead bind-mount a host directory to avoid re-downloading
~50 MB per build.

## Operational warnings

The runtimes emit structured `WARN` lines for posture issues to fix
before production traffic:

- `ZPAY_ALLOW_DEMO_PAYEE=1; running with placeholder payee <id>`: the
  placeholder-receiver gate is bypassed; local-development only.
- `ZPAY_EXPECTED_HOST unset; DPoP htu canonicalization uses inbound
  Host header.`: pin the host in production.
- `ZSPEND_PUBLIC_URL unset; DPoP htu canonicalization uses
  http://<bind addr>.`: pin the sign URL in production.
