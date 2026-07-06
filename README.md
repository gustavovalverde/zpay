# zpay

A Zcash payments stack for agents. It ships two binaries that split one
trust boundary: `zpay-runtime`, a facilitator speaking an
[x402](https://www.x402.org/)-style wire (`/x402/v2/*`) so agents and
merchants can charge for content in native ZEC, and `zspend-runtime`, a
wallet that signs agent spends under bounded, user-approved grants.
Registered payees own the offer terms, the wallet signs the spend, the
facilitator mediates the lifecycle and confirms on-chain.

The facilitator holds no funds and no spending keys: it brokers the
handoff and [zinder](https://github.com/gustavovalverde/zinder) handles
broadcast and confirmation against the chain. The wallet runtime holds
only its own sealed agent seed, never broadcasts, and signs exclusively
under a `payment_authorization` grant the user approved.

Integrating a payee or agent? Start at [Wire surface](#wire-surface) and
[Integrating as a relying party](#integrating-as-a-relying-party).
Operating a deployment? Start at [Quick start](#quick-start),
[Deploy](#deploy), and the [runbooks](docs/runbooks/).

## What it does

The facilitator runs a payment through four typed stages:

1. **Advertise.** An operator-registered payee TOML describes what each
   `(payee_id, scheme, network)` accepts: recipient address, amount, expiry
   delta, validity window.
2. **Prepare.** An agent authenticated by a key-bound request proof
   ([DPoP](https://datatracker.ietf.org/doc/html/rfc9449)) posts
   `(payee_id, scheme, network)` to `/x402/v2/prepare`. The facilitator
   resolves the offer from the registry, derives expiry from a chain-tip
   oracle, composes a domain-separated structured memo (ZIP-302)
   server-side, and returns a payment URI
   ([ZIP-321](https://zips.z.cash/zip-0321)) plus a stable `payment_id`.
3. **Settle.** The wallet signs and posts the transaction to
   `/x402/v2/settle`; zpay checks the memo version and that the signed
   expiry height matches the prepared row, broadcasts through zinder,
   and records the outcome in libSQL.
4. **Confirm.** A background oracle and a live chain-event subscription
   track confirmations and emit per-payment snapshots over Server-Sent
   Events. Statuses can regress: a payment whose block is reorged away
   returns to `broadcast` until it re-mines or its expiry lapses. A
   payment is immutable only once `settled`, meaning its block sits at
   or below zinder's settled tip. See
   [ADR-0009](docs/adrs/0009-settlement-lifecycle-and-finality.md).

For receipts, `POST /x402/v2/verify` accepts a payment disclosure
([ZIP-311](https://zips.z.cash/zip-0311)), runs the BIP-322 transparent
check in-process, and
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
| GET | `/x402/v2/payments/{id}` | none | Snapshot with `reorg_count` and `settled`; status is `awaiting`, `broadcast`, `mined`, `final`, `failed`, `never_issued`, or `expired` |
| GET | `/x402/v2/payments/{id}/events` | none | SSE stream of snapshots; closes once the payment is `settled`, `expired`, `failed`, or `never_issued` |

`/prepare` and `/settle` require a [DPoP](https://datatracker.ietf.org/doc/html/rfc9449)
proof signed by the caller's ES256 key. Idempotency is scoped by
`(jkt, idempotency_key)`, so two callers sharing an `Idempotency-Key`
header receive distinct payment IDs. A second `/settle` from a different
jkt returns `dpop_mismatch` 403.

Requests are rate limited per DPoP key on authenticated routes and per
client IP elsewhere (`ZPAY_RATE_LIMIT__PER_JKT_PER_MINUTE`, default 120;
`ZPAY_RATE_LIMIT__PER_IP_PER_MINUTE`, default 600; `0` disables a
dimension). Over the limit, responses are `429` with a `Retry-After`
header. Forwarded-for headers are ignored unless
`ZPAY_RATE_LIMIT__TRUST_FORWARDED_HEADERS` is set behind a trusted
proxy. Cross-origin browser access is off unless
`ZPAY_SERVER__CORS__ALLOWLIST` names exact origins.

## Quick start

The reproducible path is the bundled Docker image:

```bash
# Bring up zpay and the zspend wallet runtime against testnet with the
# bundled aether-demo placeholder
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
`127.0.0.1:8080`). The operations listener binds on
`ZPAY_OPS__BIND_ADDR` (default `127.0.0.1:9295`) and serves `/healthz`,
`/readyz` (a live chain probe, a store probe, and chain-view freshness,
each reported per dependency), and Prometheus `/metrics`.

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
   the lifecycle. Treat `final` as a confirmation-depth milestone, not
   settlement: a reorg can return a `mined` or `final` payment to
   `broadcast`, and the stream stays open until the payment is
   `settled` (or reaches `expired`, `failed`, or `never_issued`).

The [aether scenario in zentity's demo relying party](https://github.com/gustavovalverde/zentity/tree/main/apps/demo-rp/src/app/aether)
demonstrates the full path including a CIBA-bound user approval and a
six-character phishing-prevention code derived from the URI.

## Agent-wallet trust boundary (zspend)

`zspend-runtime` is the wallet service for autonomous agent payments. It holds
the spending seed (sealed at rest), syncs against zinder, and exposes
`POST /v1/payments/sign`. The facilitator's `/settle` is intent-blind behind a
shielded viewing key, so the wallet is the sole place the spend's intent is
checked. Before it signs, the wallet runs four checks:

1. verifies a DPoP-bound `payment_authorization` access token,
2. pins the token's `aud` to this wallet instance,
3. re-derives the `intent_hash` and matches it against the signed grant,
4. consults a revocation cache.

It then reserves the token `jti` in a durable single-use ledger,
write-then-sign, so a replay returns the cached payload and a
conflicting reuse is refused.

The wallet exposes `/v1/payments/sign`, `/v1/wallet/address`,
`/v1/capabilities`, `/.well-known/wallet-configuration`, Prometheus
`/metrics`, and a computed `/readyz` that reports JWKS reachability,
revocation-cache state, and the seed's sealing posture. See
[Proposal-0003](docs/proposals/0003-agent-wallet-production-architecture.md) for
the decision record, and the
[Aether demo](https://github.com/gustavovalverde/zentity/tree/main/apps/demo-rp/src/app/aether)
for the end-to-end flow where an issuer mints the token and zspend signs.

## Architecture

The diagram shows the facilitator plane only; the zspend wallet runtime
is a separate binary in the same workspace, described above.

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

Across the two planes, the agent obtains a signed payload from zspend's
`/v1/payments/sign`, then hands that payload to zpay's `/settle` for
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
    runbooks/               operational procedures (Railway deploy,
                              reorg recovery, zspend seed)
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
| [Runbooks](docs/runbooks/) | Railway deploy, reorg recovery, zspend seed ceremony |
| [ADR index](docs/adrs/) | Locked architectural decisions, including [ADR-0006](docs/adrs/0006-facilitator-trust-boundary.md) (trust boundary), [ADR-0008](docs/adrs/0008-compliance-authority-placement.md) (compliance authority), and [ADR-0009](docs/adrs/0009-settlement-lifecycle-and-finality.md) (settlement finality) |

## Ecosystem position

zpay sits next to two sibling projects in the Zcash agent stack:

- [zally](https://github.com/gustavovalverde/zally) is the wallet
  library; relying parties typically run zally to sign the transaction
  the facilitator hands back as a ZIP-321 URI.
- [zinder](https://github.com/gustavovalverde/zinder) is the chain
  index; zpay reads chain tip, fetches transactions, and broadcasts
  through its gRPC surface.

Both upstreams are pinned by git rev in `Cargo.toml`; bump the rev to
promote upstream changes into zpay. The pinned line is Ironwood-aware
(NU6.3): Zebra is the only full validator past activation, `/settle`
parses v5 and v6 transactions alike, and zinder stores below artifact
schema 12 must be wiped and resynced. See
[upstream platform binding](docs/architecture/upstream-platform-binding.md).

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Read [AGENTS.md](AGENTS.md) before opening a PR. Wire-shape changes are
expensive to revert; check the vocabulary spine in
[docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md)
first.
