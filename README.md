# zpay

A Zcash facilitator that speaks the [x402](https://www.x402.org/) v2 wire so
agents and merchants can charge for content with native ZEC. Registered
payees own the offer terms, the user's wallet signs the spend, zpay
mediates the lifecycle and confirms on-chain.

zpay does not custody funds and does not hold spending keys. The wallet
signs the transaction, the facilitator brokers the handoff, and
[zinder](https://github.com/gustavovalverde/zinder) handles broadcast and
confirmation against the chain.

## What it does

The facilitator runs a payment through four typed stages:

1. **Advertise.** An operator-registered payee TOML describes what each
   `(payee_id, scheme, network)` accepts: recipient address, amount, expiry
   delta, validity window.
2. **Prepare.** A DPoP-authenticated agent posts `(payee_id, scheme, network)`
   to `/x402/v2/prepare`; the facilitator resolves the offer from the
   registry, derives expiry from a chain-tip oracle, composes a
   domain-separated ZIP-302 memo server-side, and returns a ZIP-321 URI plus
   a stable `payment_id`.
3. **Settle.** The wallet signs and posts the transaction to
   `/x402/v2/settle`; zpay checks the memo version, broadcasts through
   zinder, and records the outcome in libSQL.
4. **Confirm.** A background oracle tracks confirmations and emits per-payment
   status updates over Server-Sent Events.

For receipts, `POST /x402/v2/verify` accepts a [ZIP-311](https://zips.z.cash/zip-0311)
payment disclosure, runs the BIP-322 transparent check in-process, and
reports a three-axis posture: `cryptographic_verdict`, `chain_presence`,
`amount_reconciliation`. Sapling shielded verification ships behind the
`verify_sapling` feature in a follow-on slice; today shielded disclosures
surface as `Inconclusive { unsupported_pool }`.

## Wire surface

Every JSON response body is the bare inner type. Errors follow
[RFC 7807](https://www.rfc-editor.org/rfc/rfc7807) as `application/problem+json`.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/healthz` | none | Liveness; returns `{"status":"alive"}` |
| GET | `/x402/v2/accepts?payee_id=…` | none | List the payee's accepted `(scheme, network, pay_to, amount_zat)` offers |
| GET | `/x402/v2/tip?network=…` | none | Chain-tip height the prepare path uses for expiry math |
| POST | `/x402/v2/prepare` | DPoP | Allocate a `payment_id`, return a ZIP-321 URI and memo bytes |
| POST | `/x402/v2/settle` | DPoP | Broadcast a wallet-signed transaction; jkt must match the prepare proof |
| POST | `/x402/v2/verify` | none | Verify a ZIP-311 disclosure; three-axis response |
| GET | `/x402/v2/payments/{id}` | none | Snapshot: `awaiting`, `broadcast`, `mined`, `final`, `failed`, `never_issued`, or `expired` |
| GET | `/x402/v2/payments/{id}/events` | none | SSE stream of snapshots; closes on terminal status |

`/prepare` and `/settle` require a [DPoP](https://datatracker.ietf.org/doc/html/rfc9449)
proof signed by the caller's ES256 key. Idempotency is scoped by
`(jkt, idempotency_key)`, so two callers sharing an `Idempotency-Key`
header receive distinct payment IDs. A second `/settle` from a different
jkt returns `dpop_mismatch` 403.

## Quick start

The reproducible path is the bundled Docker image:

```bash
# Bring up zpay against testnet with the bundled aether-demo placeholder
docker compose up -d
curl -s http://127.0.0.1:8080/healthz
# {"status":"alive"}
```

The compose file sets `ZPAY_ALLOW_DEMO_PAYEE=1` because the bundled
`etc/aether-demo.toml` carries a placeholder recipient address. The
runtime refuses to start with the placeholder unless this flag is
explicitly set, so a production deploy that forgets to override the
payees file fails loud at boot.

Cargo run for local development:

```bash
ZPAY_NETWORK=testnet \
ZPAY_VERIFY__NETWORK=testnet \
ZPAY_ALLOW_DEMO_PAYEE=1 \
ZPAY_PAYEES__CONFIG_PATH=$(pwd)/etc/aether-demo.toml \
cargo run --release --bin zpay-runtime
```

The HTTP listener binds on `ZPAY_SERVER__BIND_ADDR` (default
`127.0.0.1:8080`) and the operations listener binds on
`ZPAY_OPS__BIND_ADDR` (default `127.0.0.1:9295`).

## Integrating as a relying party

The expected flow for an agent or merchant BFF:

1. Register the payee in your `payees.toml`. Each entry carries one or
   more `accepts[]` templates describing `(scheme, network, pay_to,
   amount_zat, max_validity_seconds, expiry_delta_blocks?)`.
2. Mint a DPoP ES256 keypair. Persist the seed in a stable secret so the
   JKT survives process restarts and idempotency keeps working.
3. Compute a deterministic `idempotency_key` from the intent
   (user, task, item, amount). The facilitator resolves replays of the
   same key to the same `payment_id`.
4. `POST /x402/v2/prepare` with a DPoP header carrying `htm=POST`,
   `htu=https://your-zpay-host/x402/v2/prepare`, `iat` within 60s, and a
   fresh `jti`. The response is `{ payment_id, payment_uri,
   memo_bytes, expiry_height, amount_zat }`.
5. Hand `payment_uri` to the user's wallet for signing. The wallet posts
   the signed transaction back through your own surface; you forward
   it to `POST /x402/v2/settle` with a DPoP proof from the same
   keypair.
6. Subscribe to `GET /x402/v2/payments/{payment_id}/events` to observe
   the lifecycle. The stream closes when status reaches `final`,
   `failed`, `never_issued`, or `expired`.

The [aether scenario in zentity's demo relying party](https://github.com/gustavovalverde/zentity/tree/main/apps/demo-rp/src/app/aether)
demonstrates the full path including a CIBA-bound user approval and a
six-character phishing-prevention code derived from the URI.

## Agent-wallet trust boundary (zspend)

`zspend-runtime` is the wallet service for autonomous agent payments. It holds
the spending seed (sealed at rest), syncs against zinder, and exposes
`POST /v1/payments/sign`. The facilitator's `/settle` is intent-blind behind a
shielded viewing key, so the wallet is the sole place the spend's intent is
checked. Before it signs, the wallet runs four checks: it verifies a DPoP-bound
`payment_authorization` access token, pins `aud` to this wallet instance,
re-derives the `intent_hash` and matches it against the signed grant, and
consults a revocation cache. It then reserves the token `jti` write-then-sign,
so a replay returns the cached payload and a conflicting reuse is refused.

The wallet exposes `/v1/payments/sign`, `/v1/wallet/address`,
`/v1/capabilities`, `/.well-known/wallet-configuration`, and a computed
`/readyz` that reports seed, JWKS, ledger, revocation, and sealing posture. See
[Proposal-0003](docs/proposals/0003-agent-wallet-production-architecture.md) for
the decision record, and the
[Aether demo](https://github.com/gustavovalverde/zentity/tree/main/apps/demo-rp/src/app/aether)
for the end-to-end flow where an issuer mints the token and zspend signs.

## Architecture

```text
   agent / relying party (DPoP-authenticated)
        |
        v
   +----+-------------------------------------------------+
   |  zpay-runtime  (axum binary, ops listener, env-driven |
   |                  config, healthcheck on /healthz)     |
   |     +-------------------------------+                 |
   |     | zpay-x402                     |  wire adapter   |
   |     |   /accepts /tip /prepare      |  + DPoP middle  |
   |     |   /settle /verify             |  + SSE hub      |
   |     |   /payments/{id} /events      |                 |
   |     +---------------+---------------+                 |
   |                     |                                 |
   |                     v                                 |
   |  zpay-core  (prepare, settle, status, verify, binding |
   |              memo, registry, broadcast trait,         |
   |              chain-tip oracle, transaction fetcher,   |
   |              DPoP-aware PreparedTxStore)              |
   |                                                       |
   |  zpay-store (libSQL prepared_tx + settlement_ledger)  |
   +-------------------------+-----------------------------+
                             |
              +--------------+--------------+
              |                             |
              v                             v
        zinder-client                  zinder-client
        (gRPC broadcast)               (gRPC chain tip + tx fetch)
              |                             |
              v                             v
                          zinder -> Zebra
```

This diagram shows the facilitator plane. The zspend wallet runtime is a
separate binary in the same workspace: the agent obtains a signed payload from
zspend's `/v1/payments/sign`, then hands that payload to zpay's `/settle` for
broadcast.

zpay calls [zinder](https://github.com/gustavovalverde/zinder) over gRPC
for broadcast, chain-tip reads, and the transaction fetch the verify path
needs. It depends on neither zinder's HTTP surface nor the chain
directly; the only public HTTP in this loop is zpay's own.

## Repository layout

```text
zpay/
  Cargo.toml                workspace root
  Dockerfile                multi-stage Rust release build
  docker-compose.yml        local dev: zpay + dev-only payee bypass
  railway.toml              Railway deploy config; same project as zinder
  rust-toolchain.toml       1.95
  crates/
    zpay-core/              types, lifecycle, traits, ZIP-302 binding,
                              ZIP-311 verifier
    zpay-store/             libSQL impls of PreparedTxStore and
                              SettlementLedgerStore + migrations
    zpay-x402/              x402 v2 HTTP routes, SSE hub, DPoP middleware
    zpay-runtime/           binary, env-driven config, oracle wiring,
                              composition root
    zpay-e2e/               integration harness (zally as the wallet
                              counterparty)
    zspend-core/            agent-wallet trust-boundary vocabulary: the
                              payment_authorization RAR, at+jwt and DPoP
                              verification, intent-hash, SigningPolicy
    zspend-runtime/         wallet binary: sealed-seed zally wallet,
                              /v1/payments/sign, single-use jti ledger,
                              revocation check, posture gate
  etc/
    aether-demo.toml        placeholder payee config used by docker-compose
  scripts/
    deploy-to-railway.sh    self-contained Railway uploader
    test-persistence.sh     end-to-end persistence probe
    test-sse.sh             SSE wire contract probe
    test-cold-start.sh      empty-volume migration probe
    test-payee-override.sh  payees TOML bind-mount probe
    mint-dpop-proof.py      ES256 proof helper used by probes
  docs/
    product-requirements.md whole-product PRD
    architecture/           wire vocabulary, plane boundaries
    adrs/                   locked architectural decisions
    runbooks/               operational procedures (Railway deploy)
    reference/              error vocabulary
    proposals/              asks against sibling repos
```

## Deploy

Railway is the supported managed path. The runbook covers the env-var
matrix, volume provisioning, and the cross-service wiring to zinder:

```bash
./scripts/deploy-to-railway.sh
```

See [docs/runbooks/railway-deploy.md](docs/runbooks/railway-deploy.md)
for the first-deploy checklist, the placeholder-payee boot gate, and
the rollback procedure. The Dockerfile is the source of truth for the
build; Docker Compose mirrors it locally with the dev-only
`ZPAY_ALLOW_DEMO_PAYEE=1` flag flipped on.

## Validation gate

Every change in this repository must pass:

```bash
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
docker buildx build -f Dockerfile -t zpay-runtime:dev .
./scripts/test-persistence.sh
./scripts/test-sse.sh
./scripts/test-cold-start.sh
./scripts/test-payee-override.sh
```

Container probes run against the rebuilt image to lock the wire
contract; treat any change to a probe assertion as a wire-shape decision.

## Documentation

| Document | Purpose |
|---|---|
| [Product requirements](docs/product-requirements.md) | Problem, positioning, capability requirements |
| [Public interfaces](docs/architecture/public-interfaces.md) | Vocabulary spine; mandatory read before any new identifier |
| [Operational surfaces](docs/architecture/operational-surfaces.md) | Env-var schema, readiness, ops port |
| [Facilitator plane](docs/architecture/facilitator-plane.md) | Lifecycle and typed errors across boundaries |
| [Error vocabulary](docs/reference/error-vocabulary.md) | Every typed error, retry posture, operator action |
| [ADR index](docs/adrs/) | Locked architectural decisions, including [ADR-0006](docs/adrs/0006-facilitator-trust-boundary.md) (trust boundary) and [ADR-0007](docs/adrs/0007-local-zip311-verifier.md) (local verifier) |

## Ecosystem position

zpay sits next to two sibling projects in the Zcash agent stack:

- [zally](https://github.com/gustavovalverde/zally) is the wallet
  library; relying parties typically run zally to sign the transaction
  the facilitator hands back as a ZIP-321 URI.
- [zinder](https://github.com/gustavovalverde/zinder) is the chain
  index; zpay reads chain tip, fetches transactions, and broadcasts
  through its gRPC surface.

Both upstreams are pinned by git rev in `Cargo.toml`. Bump the rev to
promote upstream changes into zpay.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Read [AGENTS.md](AGENTS.md) before opening a PR. Wire-shape changes are
expensive to revert; check the vocabulary spine in
[docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md)
first.
