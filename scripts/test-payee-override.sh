#!/usr/bin/env bash
#
# scripts/test-payee-override.sh
#
# Validates the production payee-configuration override path:
# bind-mounting a custom payees.toml over /etc/zpay/payees.toml
# replaces the baked-in `aether-demo` placeholder.
#
# The probe covers:
#
#   1. The bind-mounted file is loaded ("payee registry loaded" logs
#      reflect the override payee count).
#   2. GET /zpay/v1/accepts for the overridden payee id returns the override's
#      pay_to and amount_zat.
#   3. The baked-in `aether-demo` payee is replaced (returns 404)
#      because the override REPLACES the file, it does not merge.
#   4. Wiring is bind-mount (read-only), not env-var, mirroring how
#      Railway would inject the file via a volume secret.
#
# This is the production-readiness counterpart to the bake-in default
# that ships in etc/aether-demo.toml.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
IMAGE="${ZPAY_TEST_IMAGE:-zpay-runtime:dev}"
NAME="zpay-payee-override-test-$$"
PORT="${ZPAY_TEST_PORT:-18101}"
URL="http://127.0.0.1:$PORT"
TEMP_DIR="$(mktemp -d -t zpay-override-XXXXXX)"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

OVERRIDE_PAYEE_ID="override-test-$$"
OVERRIDE_PAY_TO="utest1zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
OVERRIDE_AMOUNT=42

cat > "$TEMP_DIR/payees.toml" <<EOF
[payees."$OVERRIDE_PAYEE_ID"]
accepts = [
  { scheme = "zcash", network = "testnet", pay_to = "$OVERRIDE_PAY_TO", amount_zat = $OVERRIDE_AMOUNT, max_validity_seconds = 900 },
]
EOF

echo "[test-payee-override] starting container $NAME with bind-mounted override"
docker run --rm -d \
  --name "$NAME" \
  -p "$PORT:8080" \
  -v "$TEMP_DIR/payees.toml:/etc/zpay/payees.toml:ro" \
  "$IMAGE" >/dev/null

echo "[test-payee-override] waiting healthy (up to 60s)"
for _ in $(seq 1 30); do
  status="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo none)"
  if [[ "$status" == "healthy" ]]; then break; fi
  sleep 2
done
if [[ "$status" != "healthy" ]]; then
  echo "[test-payee-override] FAIL: container did not reach healthy; last status=$status"
  docker logs --tail 30 "$NAME"
  exit 1
fi
echo "[test-payee-override] ok: container healthy"

LOGS="$(docker logs "$NAME" 2>&1)"

if ! echo "$LOGS" | grep -q '"message":"payee registry loaded"'; then
  echo "[test-payee-override] FAIL: missing 'payee registry loaded' boot line"
  echo "$LOGS" | tail -20
  exit 1
fi
echo "[test-payee-override] ok: payee registry loaded from override path"

REGISTRY_PATH="$(echo "$LOGS" | grep -o '"path":"[^"]*"' | head -1)"
if [[ "$REGISTRY_PATH" != '"path":"/etc/zpay/payees.toml"' ]]; then
  echo "[test-payee-override] FAIL: registry loaded from unexpected path: $REGISTRY_PATH"
  exit 1
fi
echo "[test-payee-override] ok: registry path is /etc/zpay/payees.toml (the bind-mount target)"

echo "[test-payee-override] querying overridden payee"
RESPONSE="$(curl -fsS "$URL/zpay/v1/accepts?payee_id=$OVERRIDE_PAYEE_ID")"

PAY_TO_FROM_RESPONSE="$(echo "$RESPONSE" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d[0]["pay_to"])')"
AMOUNT_FROM_RESPONSE="$(echo "$RESPONSE" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d[0]["amount_zat"])')"

if [[ "$PAY_TO_FROM_RESPONSE" != "$OVERRIDE_PAY_TO" ]]; then
  echo "[test-payee-override] FAIL: /zpay/v1/accepts pay_to does not match override"
  echo "  expected: $OVERRIDE_PAY_TO"
  echo "  actual:   $PAY_TO_FROM_RESPONSE"
  exit 1
fi
echo "[test-payee-override] ok: /zpay/v1/accepts pay_to reflects override"

if [[ "$AMOUNT_FROM_RESPONSE" != "$OVERRIDE_AMOUNT" ]]; then
  echo "[test-payee-override] FAIL: /zpay/v1/accepts amount_zat=$AMOUNT_FROM_RESPONSE, expected $OVERRIDE_AMOUNT"
  exit 1
fi
echo "[test-payee-override] ok: /zpay/v1/accepts amount_zat reflects override"

echo "[test-payee-override] confirming baked-in aether-demo is REPLACED, not merged"
BAKED_STATUS="$(curl -sS -o /dev/null -w '%{http_code}' "$URL/zpay/v1/accepts?payee_id=aether-demo")"
if [[ "$BAKED_STATUS" != "404" ]]; then
  echo "[test-payee-override] FAIL: baked-in aether-demo should 404 when override file is mounted (got $BAKED_STATUS)"
  exit 1
fi
echo "[test-payee-override] ok: baked-in payee returns 404 (override replaced it)"

echo "[test-payee-override] PASS"
