# x402 local smoke test

Use this runbook to prove a local zpay stack can complete the normal x402
payment flow on Zcash testnet:

1. zpay prepares an x402 payment.
2. a real zally wallet signs the transaction.
3. zpay settles the signed bytes through zinder.
4. zpay reports the payment as mined or final.
5. zexplorer shows the transaction.

## Prerequisites

Local tools:

```bash
docker --version
cargo --version
curl --version
grpcurl --version
jq --version
```

Run all commands from the zpay repository root unless a step says otherwise.

Local services:

- a Zcash testnet zinder endpoint is running.
- zpay can reach that zinder endpoint from wherever zpay runs.
- Sapling parameters exist at `${HOME}/.local/share/ZcashParams`, or
  `ZCASH_PARAMS_HOST_DIR` points to them.

The examples below use these variables. The defaults match common localhost
ports, but any machine can override them.

```bash
export ZINDER_GRPC_URL="${ZINDER_GRPC_URL:-http://127.0.0.1:19101}"
export ZINDER_GRPC_TARGET="${ZINDER_GRPC_TARGET:-127.0.0.1:19101}"
export ZPAY_URL="${ZPAY_URL:-http://127.0.0.1:8080}"
export ZPAY_OPS_URL="${ZPAY_OPS_URL:-http://127.0.0.1:9295}"
export HARNESS_WALLET_DIR="${HARNESS_WALLET_DIR:-.tmp/zpay-e2e/harness-wallet}"
```

`ZINDER_GRPC_URL` is the URL form used by `zpay-e2e`.
`ZINDER_GRPC_TARGET` is the `host:port` form used by `grpcurl`.

Confirm zinder is reachable:

```bash
grpcurl -plaintext -d '{}' \
  "$ZINDER_GRPC_TARGET" \
  zinder.v1.wallet.WalletQuery.LatestBlock \
  | jq .
```

Choose a recent wallet birthday so the first sync is fast:

```bash
LATEST_HEIGHT=$(
  grpcurl -plaintext -d '{}' \
    "$ZINDER_GRPC_TARGET" \
    zinder.v1.wallet.WalletQuery.LatestBlock \
    | jq -r '.latestBlock.height'
)

export BIRTHDAY_HEIGHT="${BIRTHDAY_HEIGHT:-$((LATEST_HEIGHT - 500))}"
printf 'birthday_height=%s\n' "$BIRTHDAY_HEIGHT"
```

## Start zpay

This smoke test uses the zpay x402 facilitator path.

Set the chain URL to the endpoint zpay can reach from its own runtime. This may
be different from the host URL used by `grpcurl` and `zpay-e2e`.

```bash
export ZPAY_CHAIN_SOURCE_URL="${ZPAY_CHAIN_SOURCE_URL:-http://zinder-query:9101}"
```

Common values:

- `http://zinder-query:9101` when zpay and zinder share a Docker network with
  the `zinder-query` service alias.
- `http://host.docker.internal:19101` when zpay runs in Docker Desktop and
  zinder is exposed on the host.
- `$ZINDER_GRPC_URL` when zpay runs directly on the host.

The checked-in compose file references external Docker networks. Create the
shared payment network before starting zpay. The zinder stack must also create
the network that exposes the zinder service alias. If your zinder endpoint is
not exposed through that network, set `ZPAY_CHAIN_SOURCE_URL` to a reachable
endpoint and use a compose override that removes the unused external network.

```bash
docker network create zcash-x402-demo 2>/dev/null || true
docker compose up -d --build zpay
```

Check zpay readiness on the ops port:

```bash
curl -fsS "$ZPAY_OPS_URL/readyz" | jq .
```

Expected:

- `status` is `ready`.
- `dependencies.chain.live_probe` is `ok`.
- `dependencies.store.status` is `ready`.

If readiness is not ready, fix that first. The most common cause is zpay not
being able to reach zinder from its runtime, or `ZPAY_CHAIN_SOURCE_URL`
pointing at a zinder endpoint that is only reachable from the host shell.

## Create a test wallet

Create a disposable harness wallet and print a testnet Unified Address:

```bash
mkdir -p "$(dirname "$HARNESS_WALLET_DIR")"

RUST_LOG=zpay_e2e=info \
CARGO_INCREMENTAL=0 \
cargo run -p zpay-e2e --locked -- \
  --wallet-dir "$HARNESS_WALLET_DIR" \
  --zinder-url "$ZINDER_GRPC_URL" \
  --birthday "$BIRTHDAY_HEIGHT" \
  address
```

Copy the `unified_address=...` value from the output. The harness may print a
mnemonic the first time it creates the wallet. Treat this wallet as disposable
testnet state.

## Fund the wallet

Request TAZ from fauzec:

```bash
HARNESS_ADDRESS='<paste unified_address here>'

curl -sS \
  -H 'content-type: application/json' \
  --data "$(jq -n \
    --arg address "$HARNESS_ADDRESS" \
    --arg memo "zpay x402 smoke test $(date -u +%F)" \
    '{network:"testnet", address:$address, memo:$memo}')" \
  https://fauzec.com/api/v1/claim \
  | tee .tmp/zpay-e2e/fauzec-claim.json
```

Record the faucet request id and transaction id:

```bash
REQUEST_ID=$(jq -r '.request_id' .tmp/zpay-e2e/fauzec-claim.json)
TXID=$(jq -r '.txid // empty' .tmp/zpay-e2e/fauzec-claim.json)
printf 'request_id=%s txid=%s\n' "$REQUEST_ID" "$TXID"
```

If the response has `error_code: "address_on_cooldown"`, the faucet is
working but that address has already claimed recently. Wait until
`next_eligible_at_ms`, or use a new disposable wallet directory.

Poll until fauzec confirms the claim:

```bash
while true; do
  curl -fsS "https://fauzec.com/api/v1/status/testnet/$REQUEST_ID" \
    | tee .tmp/zpay-e2e/fauzec-status.json \
    | jq '{state, outcome, txid, confirmed_height, error_code}'

  state=$(jq -r '.state' .tmp/zpay-e2e/fauzec-status.json)
  test "$state" = "confirmed" && break
  sleep 15
done

TXID=$(jq -r '.txid' .tmp/zpay-e2e/fauzec-status.json)
```

Check explorer visibility:

```bash
curl -fsSI "https://zexplorer.app/testnet/tx/$TXID" | sed -n '1,12p'
```

Expected: HTTP 200.

Wait until the wallet sees spendable funds. zally reports shielded balances
only after the note is spendable; on testnet this currently means about 10
confirmations.

```bash
while true; do
  RUST_LOG=zpay_e2e=info \
  CARGO_INCREMENTAL=0 \
  cargo run -q -p zpay-e2e --locked -- \
    --wallet-dir "$HARNESS_WALLET_DIR" \
    --zinder-url "$ZINDER_GRPC_URL" \
    --birthday "$BIRTHDAY_HEIGHT" \
    status \
    2>&1 | tee .tmp/zpay-e2e/wallet-status.log

  grep -Eq 'ironwood_zat=[1-9][0-9]*|orchard_zat=[1-9][0-9]*|sapling_zat=[1-9][0-9]*' \
    .tmp/zpay-e2e/wallet-status.log && break

  sleep 15
done
```

Expected output includes a non-zero shielded balance, usually
`ironwood_zat=100000000` for a fresh fauzec claim.

## Run the x402 flow

Run the full x402 smoke test:

```bash
RUST_LOG=zpay_e2e=info \
CARGO_INCREMENTAL=0 \
cargo run -p zpay-e2e --locked -- \
  --wallet-dir "$HARNESS_WALLET_DIR" \
  --zpay-url "$ZPAY_URL" \
  --zinder-url "$ZINDER_GRPC_URL" \
  --birthday "$BIRTHDAY_HEIGHT" \
  run \
  --payee-id aether-demo \
  --poll-seconds 600
```

The command should print a `payment_id` and transaction id. It passes when
zpay observes the transaction mined before `--poll-seconds` expires.

## Verify the payment

Check zpay status:

```bash
PAYMENT_ID='<paste payment_id here>'

curl -fsS "$ZPAY_URL/x402/v2/payments/$PAYMENT_ID" | jq .
```

Expected:

- `status` is `mined` or `final`.
- `broadcast_outcome.kind` is `accepted` or `duplicate`.
- `broadcast_outcome.transaction_id` is present.
- `mined_block_height` is present.
- `confirmation_count` is at least `1`.

Check zexplorer:

```bash
PAYMENT_TXID=$(
  curl -fsS "$ZPAY_URL/x402/v2/payments/$PAYMENT_ID" \
    | jq -r '.broadcast_outcome.transaction_id'
)

curl -fsSI "https://zexplorer.app/testnet/tx/$PAYMENT_TXID" | sed -n '1,12p'
```

Expected: HTTP 200.

`status: "final"` means the transaction reached the configured finality depth.
It is not the same as `settled: true`; `settled` becomes true only after the
mined block is at or below zinder's settled tip.

## Useful troubleshooting

**`/readyz` is not ready.** Check that zinder is running, that zpay joined the
Docker network that exposes zinder when using an in-network alias, and that
`ZPAY_CHAIN_SOURCE_URL` points at an endpoint zpay can reach from its runtime.

**Fauzec returns `address_on_cooldown`.** The address already received a drip
within the cooldown window. Use a new disposable wallet directory or wait until
`next_eligible_at_ms`.

**Wallet status stays at zero.** The faucet transaction may not have enough
confirmations yet, or the configured zinder tip may not have indexed the
confirmed block. Check the faucet tx with:

```bash
grpcurl -plaintext \
  -d "$(jq -n --arg txid "$TXID" '{transaction_id:$txid}')" \
  "$ZINDER_GRPC_TARGET" \
  zinder.v1.wallet.WalletQuery.Transaction \
  | jq .
```

**`zpay-e2e run` reports insufficient balance.** Re-run `zpay-e2e status` and
wait until one of the shielded balance fields is non-zero.

**zexplorer shows the tx before zpay does.** zexplorer and configured zinder
can be at slightly different tips. zpay uses configured zinder as the chain
authority, so wait for zinder to catch up.

## What to record

For a normal smoke-test report, capture:

- zpay commit.
- zinder latest block height.
- fauzec request id and faucet txid.
- `zpay-e2e run` payment id and txid.
- final `GET /x402/v2/payments/{payment_id}` response.
- zexplorer transaction URL.
