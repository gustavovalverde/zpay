#!/usr/bin/env bash
#
# scripts/test-persistence.sh
#
# Validates that the libSQL-backed runtime persists prepared-tx rows
# and the idempotency index across a kill + restart cycle.
#
# Does not require zinder, fauzec, zentity, or a real chain plane. Run
# this whenever you touch the persistence layer to confirm the wire
# surface still reads and writes through libSQL across process
# restarts.
#
# Exits 0 on success, 1 on any persistence check that fails.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Build once. `cargo build --release` is fast on rebuild; first run
# takes a few minutes. Run cargo from REPO_ROOT so it picks up the
# repo's `rust-toolchain.toml` rather than the current shell's.
echo "[test-persistence] building zpay-runtime..."
(cd "$REPO_ROOT" && cargo build --release -p zpay-runtime > /dev/null)

BINARY="$REPO_ROOT/target/release/zpay-runtime"

# Distinct ports + libSQL file under a tempdir so this run can't collide
# with any other zpay instance.
TEMP_DIR="$(mktemp -d -t zpay-persistence-XXXXXX)"
APP_PORT=17402
OPS_PORT=17403
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

# DPoP keypair used across every probe call. The (jkt, idempotency_key)
# idempotency composite needs the jkt to stay constant within one run;
# only the proof's jti and iat vary per call.
DPOP_KEYFILE="$TEMP_DIR/dpop-key.pem"
python3 "$SCRIPT_DIR/mint-dpop-proof.py" --init "$DPOP_KEYFILE"

mint_proof() {
  local method="$1"
  local path="$2"
  python3 "$SCRIPT_DIR/mint-dpop-proof.py" \
    --keyfile "$DPOP_KEYFILE" \
    --method "$method" \
    --url "$APP_URL$path" \
    --jti "jti-$RANDOM-$RANDOM-$(date +%s%N)"
}

# A payee the harness can target. Recipient address is a placeholder
# because we never settle a tx; only `/prepare` and `/payments/{id}`
# are exercised.
cat > "$TEMP_DIR/payees.toml" <<'EOF'
[payees."durability-test"]
accepts = [
  { scheme = "zcash", network = "testnet", pay_to = "utest1placeholder", amount_zat = 10000, max_validity_seconds = 600 },
]
EOF

start_runtime() {
  local label="$1"
  ZPAY_SERVER__BIND_ADDR=127.0.0.1:$APP_PORT \
  ZPAY_OPS__BIND_ADDR=127.0.0.1:$OPS_PORT \
  ZPAY_NETWORK=testnet \
  ZPAY_VERIFY__NETWORK=testnet \
  ZPAY_PAYEES__CONFIG_PATH="$TEMP_DIR/payees.toml" \
  ZPAY_STORE__BACKEND=libsql \
  ZPAY_STORE__URL="$LIBSQL_URL" \
  "$BINARY" > "$TEMP_DIR/runtime-$label.log" 2>&1 &
  ZPAY_PID=$!
  # Wait up to 10s for the /x402/v2/accepts endpoint to answer.
  for _ in $(seq 1 50); do
    if curl -fsS "$APP_URL/x402/v2/accepts?payee_id=durability-test" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "[test-persistence] runtime did not come up; tail of log:"
  tail -20 "$TEMP_DIR/runtime-$label.log"
  return 1
}

stop_runtime() {
  if [[ -n "${ZPAY_PID:-}" ]]; then
    kill "$ZPAY_PID" >/dev/null 2>&1 || true
    wait "$ZPAY_PID" 2>/dev/null || true
    ZPAY_PID=""
  fi
}

prepare_body() {
  local idempotency_key="$1"
  cat <<EOF
{
  "payee_id": "durability-test",
  "network": "testnet",
  "scheme": "zcash",
  "resource_uri": "durability-test/items/probe",
  "nonce": "$idempotency_key",
  "idempotency_key": "$idempotency_key"
}
EOF
}

call_prepare() {
  local key="$1"
  local proof
  proof="$(mint_proof POST /x402/v2/prepare)"
  curl -fsS -X POST "$APP_URL/x402/v2/prepare" \
    -H 'content-type: application/json' \
    -H "DPoP: $proof" \
    -d "$(prepare_body "$key")"
}

call_status() {
  local payment_id="$1"
  curl -fsS "$APP_URL/x402/v2/payments/$payment_id"
}

assert() {
  local description="$1"
  local actual="$2"
  local expected="$3"
  if [[ "$actual" != "$expected" ]]; then
    echo "[test-persistence] FAIL: $description"
    echo "  expected: $expected"
    echo "  actual:   $actual"
    exit 1
  fi
  echo "[test-persistence] ok: $description"
}

echo "[test-persistence] phase 1: start runtime, prepare once, verify"
start_runtime run1

RESPONSE_1="$(call_prepare durability-001)"
PAYMENT_ID="$(echo "$RESPONSE_1" | jq -r '.payment_id')"
test -n "$PAYMENT_ID" || { echo "[test-persistence] FAIL: no payment_id in response"; exit 1; }
echo "[test-persistence] prepared payment_id=$PAYMENT_ID"

STATUS_1="$(call_status "$PAYMENT_ID" | jq -r '.status')"
assert "status after first prepare" "$STATUS_1" "awaiting"

RESPONSE_2="$(call_prepare durability-001)"
PAYMENT_ID_2="$(echo "$RESPONSE_2" | jq -r '.payment_id')"
assert "idempotency replay returns same payment_id" "$PAYMENT_ID_2" "$PAYMENT_ID"

echo "[test-persistence] phase 2: restart runtime against same libSQL file"
stop_runtime
start_runtime run2

STATUS_2="$(call_status "$PAYMENT_ID" | jq -r '.status')"
assert "status survives restart" "$STATUS_2" "awaiting"

RESPONSE_3="$(call_prepare durability-001)"
PAYMENT_ID_3="$(echo "$RESPONSE_3" | jq -r '.payment_id')"
assert "idempotency index survives restart" "$PAYMENT_ID_3" "$PAYMENT_ID"

echo "[test-persistence] phase 3: distinct idempotency key allocates a fresh payment_id"
RESPONSE_4="$(call_prepare durability-002)"
PAYMENT_ID_4="$(echo "$RESPONSE_4" | jq -r '.payment_id')"
if [[ "$PAYMENT_ID_4" == "$PAYMENT_ID" ]]; then
  echo "[test-persistence] FAIL: distinct idempotency keys collided onto $PAYMENT_ID"
  exit 1
fi
echo "[test-persistence] ok: distinct idempotency key produced $PAYMENT_ID_4"

echo "[test-persistence] PASS"
