# zpay

Zcash-native payments facilitator: x402 v2 and MPP wire adapters over one
protocol-neutral core, settling shielded ZEC for AI agents and apps.

## What it does

- Speaks x402 v2 over HTTPS so any agent or merchant that already talks
  Coinbase x402 can pay or accept ZEC with one header swap.
- Speaks MPP (Machine Payments Protocol) over the same protocol-neutral core;
  adding the second adapter does not touch the first.
- Holds a short-TTL prepared-transaction cache so user-signed unbroadcast
  transactions survive the round trip between agent, merchant, and
  facilitator without ever exposing spending keys to zpay.
- Settles by calling [zinder](https://github.com/gustavovalverde/zinder)'s
  `BroadcastTransaction` RPC and confirms by subscribing to its
  `ChainEvents` stream.
- Validates [zentity](https://app.zentity.xyz) Proof-of-Human tokens
  (SD-JWT-VC, EdDSA) so merchants enforce compliance posture without
  handling PII.

zpay is the **payments protocol layer**, not a wallet. It never holds user
spending keys; the user's wallet signs the unbroadcast transaction and zpay
holds the result for at most a few minutes before broadcasting on settle.
See [ADR-0001](docs/adrs/0001-workspace-and-crate-boundaries.md).

## Architecture at a glance

```text
agent / merchant / app
       |
       v
 +-----+------------------------------------------------+
 | zpay-runtime  (HTTP, axum, /openapi.json, /healthz)  |
 |    +--------------+  +--------------+                |
 |    | zpay-x402    |  | zpay-mpp     | wire adapters  |
 |    +------+-------+  +------+-------+                |
 |           |                 |                        |
 |           +--------+--------+                        |
 |                    v                                 |
 |        zpay-core (prepare, oracle, broadcast,        |
 |                   compliance, capability)            |
 |        zpay-store (libSQL prepared-tx + ledger)      |
 +--------------------+---------------------------------+
                      |
       +--------------+--------------+
       |                             |
       v                             v
+--------------+              +---------------+
| zally        |              | zinder-client |
| (embedded)   |              | (gRPC)        |
+------+-------+              +-------+-------+
       |                              |
       v                              v
   wallet ops                  zinder -> Zebra
```

zpay embeds [zally](https://github.com/gustavovalverde/zally) as a library
to construct, parse, and validate ZIP-321 payment requests. It calls
[zinder](https://github.com/gustavovalverde/zinder) over gRPC for broadcast
and confirmation. It depends on neither's HTTP surface; the only public HTTP
in this loop is zpay's own.

## Quickstart

Bring up Zebra and zinder against testnet (the
[z3](https://github.com/gustavovalverde/z3) stack handles both), then run:

```bash
ZPAY_NETWORK=testnet \
ZPAY_NODE__INDEXER_GRPC_ADDR=http://127.0.0.1:9101 \
ZPAY_WALLET__AGE_IDENTITY_TEXT="$(cat ~/.config/zpay/seed.age-identity)" \
cargo run --bin zpay-runtime
```

The HTTP listener binds on `ZPAY_HTTP__BIND_ADDR` (default `127.0.0.1:8080`)
and exposes `/openapi.json` for machine discovery, `/healthz` and
`/readyz` for operations, and `/x402/v2/*` and (when feature-enabled)
`/mpp/v1/*` for paying agents.

## Repository layout

```text
zpay/
  Cargo.toml                 workspace
  rust-toolchain.toml        1.95.0
  clippy.toml                identifier and complexity discipline
  deny.toml                  dependency policy
  .github/workflows/         CI gate
  .config/nextest.toml       test profiles
  crates/
    zpay-core/               types, prepare, oracle, broadcast, compliance
    zpay-store/              libSQL prepared-tx cache and settlement ledger
    zpay-x402/               x402 v2 wire adapter
    zpay-mpp/                MPP wire adapter (Phase 5; feature-gated)
    zpay-runtime/            HTTP binary, ops listener, env-driven config
    zpay-testkit/            fixtures, live-test gates
  deploy/                    Dockerfile + railway.toml
  docs/
    product-requirements.md  whole-product PRD
    architecture/            vocabulary spine and boundary contracts
    adrs/                    locked decisions
    reference/               error vocabulary
    proposals/               asks against upstream sibling repos
    plans/                   per-slice executable phasing
    runbooks/                operational procedures
```

## Where zpay fits

| Audience | What zpay gives you |
|---|---|
| Agents | One x402 or MPP endpoint per merchant; ZEC settlement with confirmation polling or webhook. No wallet code. |
| Merchants | A facilitator that validates Proof-of-Human compliance, holds the user-signed transaction, broadcasts, and verifies receipt. |
| Wallet integrators | An OpenAPI spec at `/openapi.json` and a typed gRPC surface (Phase 6). |
| Operators | One process, libSQL persistence, structured-JSON tracing, ops port with `/healthz` + `/readyz` + `/metrics`. |

## Validation gate

Every change in this repository must pass:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --profile=ci
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps
cargo deny check
cargo machete
```

Live tests (T3) opt in:

```bash
ZPAY_TEST_LIVE=1 ZPAY_NETWORK=regtest cargo nextest run --profile=ci-live --run-ignored=all
```

Mainnet live tests additionally require `ZPAY_TEST_ALLOW_MAINNET=1`.
Production binaries strip every key starting with `ZPAY_TEST_` from env reads.

## Documentation

- [Product requirements](docs/product-requirements.md): problem, positioning,
  capability requirements by surface, milestones, open questions.
- [Public interfaces](docs/architecture/public-interfaces.md): vocabulary spine;
  mandatory read before any new identifier.
- [Operational surfaces](docs/architecture/operational-surfaces.md):
  readiness state machine, ops port, env-var schema, live-test gates.
- [Facilitator plane](docs/architecture/facilitator-plane.md): prepare,
  settle, watch, verify lifecycle and the typed errors at each boundary.
- [Upstream platform binding](docs/architecture/upstream-platform-binding.md):
  what zpay expects from zally, zinder, and zentity.
- [Error vocabulary](docs/reference/error-vocabulary.md): every typed error
  with retry posture and operator action.
- [ADR index](docs/README.md): locked architectural decisions.

## Ecosystem position

zpay is the payments-protocol peer to
[zally](https://github.com/gustavovalverde/zally) (wallet library) and
[zinder](https://github.com/gustavovalverde/zinder) (chain index). Both
upstreams are consumed by pinned git rev; bump the rev in
`Cargo.toml` to promote upstream changes into zpay.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Read [AGENTS.md](AGENTS.md) before opening a PR. Vocabulary breaks are
expensive to revert; check the spine first.
