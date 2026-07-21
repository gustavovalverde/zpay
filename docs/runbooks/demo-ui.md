# Browser checkout demo

Use this runbook to show the normal zpay Zcash payment lifecycle through a
browser:

1. the report starts locked,
2. zpay prepares a ZEC payment,
3. the demo gateway signs through checkout or autopay mode,
4. zpay settles through zinder,
5. the UI unlocks the report and links to zexplorer.

The browser talks only to `zpay-demo` under `/demo/v1/*`. It never receives
wallet seed material, DPoP private keys, issuer keys, access tokens, or signed
transaction bytes.

## Prerequisites

Install local tools:

```bash
cargo --version
pnpm --version
openssl version
jq --version
```

Run all commands from the zpay repository root unless a step says otherwise.

Start or provide:

- zinder on Zcash testnet,
- `zpay-runtime` configured for that zinder endpoint,
- `zspend-runtime` configured for the same network if you want autopay,
- a payee registered in zpay. The default demo payee is `aether-demo`.

For the base stack, follow
[end-to-end-validation.md](end-to-end-validation.md) through the zpay readiness
check first.

## Configure URLs

The defaults match the local developer ports:

```bash
export ZPAY_DEMO_ZPAY_URL="${ZPAY_DEMO_ZPAY_URL:-http://127.0.0.1:8080}"
export ZPAY_DEMO_ZPAY_PUBLIC_URL="${ZPAY_DEMO_ZPAY_PUBLIC_URL:-$ZPAY_DEMO_ZPAY_URL}"
export ZPAY_DEMO_ZPAY_OPS_URL="${ZPAY_DEMO_ZPAY_OPS_URL:-http://127.0.0.1:9295}"
export ZPAY_DEMO_ZSPEND_URL="${ZPAY_DEMO_ZSPEND_URL:-http://127.0.0.1:8090}"
export ZPAY_DEMO_ZSPEND_PUBLIC_URL="${ZPAY_DEMO_ZSPEND_PUBLIC_URL:-$ZPAY_DEMO_ZSPEND_URL}"
export ZPAY_DEMO_ZINDER_URL="${ZPAY_DEMO_ZINDER_URL:-http://127.0.0.1:19102}"
export ZPAY_DEMO_WALLET_DIR="${ZPAY_DEMO_WALLET_DIR:-.tmp/zpay-demo/wallet}"
export ZPAY_DEMO_NETWORK="${ZPAY_DEMO_NETWORK:-testnet}"
export ZPAY_DEMO_PAYEE_ID="${ZPAY_DEMO_PAYEE_ID:-aether-demo}"
```

`ZPAY_DEMO_NETWORK=mainnet` is refused. The demo gateway is a local testnet
surface. Leave `ZPAY_DEMO_BIRTHDAY_HEIGHT` unset for fresh demo wallets; the
gateway chooses the current zinder tip minus 500 blocks so first-run sync stays
short. Set it only when restoring an older wallet.

## Configure autopay

Skip this section if you only need checkout mode.

`zspend-runtime` verifies access tokens with `ZSPEND_JWKS_FILE`. The demo
gateway mints those tokens with its local dev issuer key. If
`ZPAY_DEMO_ISSUER_KEY_PATH` is unset, the gateway creates a P-256 issuer at:

- `.tmp/zpay-demo/wallet/dev-issuer-p256.pem`
- `.tmp/zpay-demo/wallet/dev-jwks.json`

Start the gateway once to materialise those files, then start or restart
`zspend-runtime` with:

```bash
export ZSPEND_JWKS_FILE="$(pwd)/.tmp/zpay-demo/wallet/dev-jwks.json"
export ZSPEND_AUDIENCE="urn:zpay:zspend:local-dev"
export ZPAY_DEMO_ZSPEND_AUDIENCE="$ZSPEND_AUDIENCE"
```

If zspend already trusts another dev issuer, set
`ZPAY_DEMO_ISSUER_KEY_PATH` and `ZPAY_DEMO_ISSUER_KID` to the matching private
key and JWKS `kid`. Its `/readyz` response should show `jwks_cache: "loaded"`
before autopay is expected to work.

## Start the demo gateway

Run the gateway:

```bash
RUST_LOG=zpay_demo=info,zally=info \
cargo run -p zpay-demo --locked
```

Check readiness:

```bash
curl -fsS http://127.0.0.1:7410/demo/v1/readiness | jq .
```

Expected:

- `zpay.status` is `ready`,
- `zinder.status` is `ready`,
- `wallet.status` is `ready` or `needs_funds`,
- `zspend.status` is `ready` before autopay.

## Fund the demo wallet

Print the demo wallet address:

```bash
curl -fsS http://127.0.0.1:7410/demo/v1/wallet | jq -r '.address'
```

Fund that address from the UI with `Use faucet`, or from the terminal:

```bash
DEMO_ADDRESS="$(
  curl -fsS http://127.0.0.1:7410/demo/v1/wallet | jq -r '.address'
)"

curl -sS \
  -H 'content-type: application/json' \
  --data "$(jq -n \
    --arg address "$DEMO_ADDRESS" \
    '{network:"testnet", address:$address, memo:"zpay demo UI"}')" \
  https://fauzec.com/api/v1/claim \
  | tee .tmp/zpay-demo/fauzec-claim.json
```

If fauzec returns a transaction id, open it in zexplorer:

```bash
TXID="$(jq -r '.txid // empty' .tmp/zpay-demo/fauzec-claim.json)"
test -n "$TXID" && open "https://zexplorer.app/testnet/tx/$TXID"
```

Wait until `GET /demo/v1/wallet` reports `is_funded: true`.

## Start the browser app

Install and run the standalone Vite app:

```bash
pnpm --dir demo install
pnpm --dir demo dev
```

Open [http://127.0.0.1:5174](http://127.0.0.1:5174).

## Run checkout mode

1. Select `Checkout`.
2. Click `Pay with ZEC`.
3. Review the wallet sheet.
4. Click `Approve payment`.
5. Watch the stepper move through Quote, Authorize, Broadcast, and Settled.
6. Click `View transaction`.

Expected:

- the locked report becomes `Report unlocked`,
- the transaction opens on zexplorer,
- `What's happening on the wire` shows the payment id, expiry height,
  confirmations, and transaction id.

The demo grants access once zpay reports the payment as `final`. zpay may still
report `settled: false` until zinder's settled tip reaches the mined block.

## Read the protocol receipt

Select the `Receipt` tab at any point during or after a checkout to see the
same payment as a lifecycle timeline (prepared, signed, broadcast, settled).
Switching tabs does not interrupt the in-flight payment: the checkout view's
state, including its open SSE subscription, stays alive underneath. Each
timeline node shows a client-observed timestamp only for stages the browser
actually witnessed; a stage the gateway's response skipped over (for example
when settlement jumps straight from `review` to a later stage) is marked
reached without a fabricated timestamp.

## Run autopay mode

1. Confirm zspend `/readyz` is ready and `jwks_cache` is `loaded`.
2. Select `Autopay`.
3. Click `Pay with ZEC`.
4. Click `Start autopay`.
5. Watch the stepper move through Quote, Authorize, Broadcast, and Settled.
6. Click `View transaction`.

Expected:

- the gateway mints a local dev `payment_authorization`,
- zspend signs the prepared ZIP-321 payment,
- zpay settles the signed transaction,
- the report unlocks after confirmation.

The same access rule applies here: `final` unlocks the demo report, while
settled depth can arrive later.

## Run the agent comparison

Select the `Agent` tab and click `Run agent`. This drives real, sequential
`prepare` and `settle` calls in autopay mode against the demo stack: each call
mints its own single-use zspend authorization grant (zspend does not pool a
budget across payments), and the loop stops at a small demo-configured call
count or spend ceiling, whichever comes first. `Reset` clears the counters so
the loop can run again. This mode exercises the same autopay path as above,
so it needs zspend `/readyz` ready and `jwks_cache` loaded.

## Browse receipts and verify a disclosure

Select the `Receipts` tab to see every payment made this session
(`GET /demo/v1/payments`), most recent first; the list resets when the
gateway restarts, since it reflects only the gateway's in-memory state. Pick
a payment and click `Verify payment disclosure` (enabled once the payment
has broadcast) to call `POST /demo/v1/payments/{payment_id}/verify`. The
gateway resolves the expected transaction, recipient, amount, and challenge
from its payment record instead of trusting browser-supplied expectations.
The demo wallet produces ZIP-311 Draft1 for Sapling payments and the
explicitly versioned Zally Ironwood extension for Unified Address payments.
The verifier checks the disclosure against the mined transaction and reports
the cryptographic, chain-presence, amount, recipient, and challenge verdicts
independently.

## Facilitator console (operator view)

Select the `Console` tab for an operator-facing view of zpay-runtime itself:
recent settled payments across all payees, rate-limit budgets and current
load. This proxies to zpay-runtime's ops-listener `GET /payments`
(see [ADR-0014](../adrs/0014-operator-payments-console.md)), not to
`zpay-demo`'s own state, so it requires a zpay-runtime build that includes
this route. Against an older zpay-runtime, the tab shows a graceful "zpay ops
/payments returned 404" error rather than crashing; rebuild and restart
zpay-runtime to pick up the route.

## Troubleshooting

| UI message | Next check |
|------------|------------|
| `The demo wallet needs testnet funds` | Fund the address with fauzec, then wait for wallet sync. |
| `zpay can't reach zinder. Check readiness, then try again.` | Check zpay `/readyz` and `ZPAY_CHAIN_SOURCE_URL`. |
| `zspend isn't ready. Wait for sync, then try again.` | Check zspend `/readyz`, wallet sync freshness, and `ZSPEND_JWKS_FILE`. |
| `This payment expired. Start a new checkout` | Start a new payment; the prior expiry height has passed. |

## Validation

Run the focused gates:

```bash
cargo test -p zpay-demo --locked
pnpm --dir demo typecheck
pnpm --dir demo test
pnpm --dir demo build
pnpm --dir demo test:e2e
```

The live demo gate is manual: complete checkout and autopay against the local
testnet stack, then open the txid in zexplorer.
