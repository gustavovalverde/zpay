#!/usr/bin/env bash
#
# scripts/test-sse.sh
#
# Validates the GET /x402/v2/payments/{id}/events SSE endpoint:
#
# 1. Subscribing to a known payment_id streams a snapshot event with
#    the canonical JSON shape (no `{ data: }` envelope).
# 2. Anti-buffering headers are set for reverse proxies.
# 3. Subscribing to an unknown payment_id returns 404 (hub-leak gate).
#
# Companion to test-persistence.sh. Run both before any change to the
# zpay-x402 events module or the zpay-runtime oracle wiring.
#
# Exits 0 on success, 1 on any check that fails.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "[test-sse] building zpay-runtime..."
(cd "$REPO_ROOT" && cargo build --release -p zpay-runtime > /dev/null)

BINARY="$REPO_ROOT/target/release/zpay-runtime"

TEMP_DIR="$(mktemp -d -t zpay-sse-XXXXXX)"
APP_PORT=17406
OPS_PORT=17407
APP_URL="http://127.0.0.1:$APP_PORT"
LIBSQL_URL="file:$TEMP_DIR/zpay.libsql"

cleanup() {
  if [[ -n "${ZPAY_PID:-}" ]]; then
    kill "$ZPAY_PID" >/dev/null 2>&1 || true
    wait "$ZPAY_PID" 2>/dev/null || true
  fi
  rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

DPOP_KEYFILE="$TEMP_DIR/dpop-key.pem"
python3 "$SCRIPT_DIR/mint-dpop-proof.py" --init "$DPOP_KEYFILE"

cat > "$TEMP_DIR/payees.toml" <<'EOF'
[payees."sse-test"]
accepts = [
  { scheme = "zcash", network = "testnet", pay_to = "utest1placeholder", amount_zat = 10000, max_validity_seconds = 600 },
]
EOF

ZPAY_SERVER__BIND_ADDR=127.0.0.1:$APP_PORT \
ZPAY_OPS__BIND_ADDR=127.0.0.1:$OPS_PORT \
ZPAY_NETWORK=testnet \
ZPAY_VERIFY__NETWORK=testnet \
ZPAY_PAYEES__CONFIG_PATH="$TEMP_DIR/payees.toml" \
ZPAY_STORE__BACKEND=libsql \
ZPAY_STORE__URL="$LIBSQL_URL" \
"$BINARY" > "$TEMP_DIR/runtime.log" 2>&1 &
ZPAY_PID=$!

for _ in $(seq 1 50); do
  if curl -fsS "$APP_URL/x402/v2/accepts?payee_id=sse-test" >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done

PREP_BODY="$(cat <<'EOF'
{
  "payee_id": "sse-test",
  "network": "testnet",
  "scheme": "zcash",
  "resource_uri": "sse-test/items/probe",
  "nonce": "sse-known",
  "idempotency_key": "sse-known"
}
EOF
)"

PROOF="$(python3 "$SCRIPT_DIR/mint-dpop-proof.py" \
  --keyfile "$DPOP_KEYFILE" \
  --method POST \
  --url "$APP_URL/x402/v2/prepare" \
  --jti "sse-jti-$$")"

PAYMENT_ID="$(curl -fsS -X POST "$APP_URL/x402/v2/prepare" \
  -H 'content-type: application/json' \
  -H "DPoP: $PROOF" \
  -d "$PREP_BODY" | jq -r '.payment_id')"
test -n "$PAYMENT_ID" || { echo "[test-sse] FAIL: no payment_id"; exit 1; }
echo "[test-sse] prepared payment_id=$PAYMENT_ID"

# Capture the headers + the first SSE event. curl returns exit 28 when
# --max-time fires; we accept that because the stream is non-terminal.
STREAM_OUT="$TEMP_DIR/stream.out"
curl -sS -N --max-time 2 -i "$APP_URL/x402/v2/payments/$PAYMENT_ID/events" > "$STREAM_OUT" 2>/dev/null || true

assert_contains() {
  local description="$1"
  local needle="$2"
  if grep -qF -- "$needle" "$STREAM_OUT"; then
    echo "[test-sse] ok: $description"
  else
    echo "[test-sse] FAIL: $description"
    echo "  expected to find: $needle"
    echo "  in stream output:"
    sed 's/^/    /' "$STREAM_OUT"
    exit 1
  fi
}

assert_contains "Content-Type: text/event-stream" "content-type: text/event-stream"
assert_contains "Cache-Control disables buffering" "cache-control: no-cache, no-transform"
assert_contains "X-Accel-Buffering disables nginx buffering" "x-accel-buffering: no"
assert_contains "snapshot event name" "event: snapshot"
assert_contains "snapshot payload is raw JSON (no data envelope)" "\"payment_id\":\"$PAYMENT_ID\""
assert_contains "initial status is awaiting" "\"status\":\"awaiting\""
assert_contains "intent_posture defaults to unverified" "\"intent_posture\":\"unverified\""

# Unknown payment_id must return 404 before any hub entry is created.
UNKNOWN_STATUS="$(curl -fsS -o /dev/null -w '%{http_code}' "$APP_URL/x402/v2/payments/never-prepared/events" || true)"
if [[ "$UNKNOWN_STATUS" != "404" ]]; then
  echo "[test-sse] FAIL: unknown payment_id should return 404 (got $UNKNOWN_STATUS)"
  exit 1
fi
echo "[test-sse] ok: unknown payment_id returns 404 (hub-leak gate)"

echo "[test-sse] PASS"
