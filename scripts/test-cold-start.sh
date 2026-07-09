#!/usr/bin/env bash
#
# scripts/test-cold-start.sh
#
# Validates that the zpay-runtime container boots cleanly from an
# empty volume (the Railway-deploy and first-time-developer paths):
#
#   1. Schema migrations apply from version 0; `zpay_schema_migrations`
#      ends at version 1.
#   2. The /zpay/v1/prepare endpoint accepts a request and returns a
#      ZIP-321 URI on a freshly migrated database.
#   3. The container reports healthy.
#
# Companion to test-persistence.sh (which exercises the populated-
# volume restart path). This script is the populated-volume case's
# adversary: prove migrations run from scratch.
#
# Uses `docker run --rm` so the libSQL file lives in the ephemeral
# container fs and never touches the compose-managed `zpay-data`
# volume; the running compose stack is undisturbed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
IMAGE="${ZPAY_TEST_IMAGE:-zpay-runtime:dev}"
NAME="zpay-cold-start-test-$$"
PORT="${ZPAY_TEST_PORT:-18100}"
URL="http://127.0.0.1:$PORT"
EXPECTED_SCHEMA_VERSION="$(
  find "$REPO_ROOT/crates/zpay-store/migrations" -maxdepth 1 -name '[0-9][0-9][0-9][0-9]_*.sql' \
    | sed -E 's#.*/0*([0-9]+)_.*#\1#' \
    | sort -n \
    | tail -1
)"

TEMP_DIR="$(mktemp -d -t zpay-cold-start-XXXXXX)"
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

DPOP_KEYFILE="$TEMP_DIR/dpop-key.pem"
python3 "$SCRIPT_DIR/mint-dpop-proof.py" --init "$DPOP_KEYFILE"

echo "[test-cold-start] starting container $NAME from $IMAGE (no volume)"
# ZPAY_ALLOW_DEMO_PAYEE=1 bypasses the placeholder-receiver boot gate
# so this probe exercises the baked-in `aether-demo` payee. Production
# stacks must NOT set this; the runbook spells out the contract.
docker run --rm -d --name "$NAME" -p "$PORT:8080" \
  -e ZPAY_VERIFY__NETWORK=testnet \
  -e ZPAY_ALLOW_DEMO_PAYEE=1 \
  "$IMAGE" >/dev/null

echo "[test-cold-start] waiting healthy (up to 60s)"
for _ in $(seq 1 30); do
  status="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo none)"
  if [[ "$status" == "healthy" ]]; then
    echo "[test-cold-start] ok: container reports healthy"
    break
  fi
  sleep 2
done
if [[ "$status" != "healthy" ]]; then
  echo "[test-cold-start] FAIL: container did not reach healthy state; last status=$status"
  docker logs --tail 50 "$NAME"
  exit 1
fi

echo "[test-cold-start] inspecting boot logs for migration signal"
LOGS="$(docker logs "$NAME" 2>&1)"

if ! echo "$LOGS" | grep -q '"message":"libsql schema migrations applied"'; then
  echo "[test-cold-start] FAIL: missing 'libsql schema migrations applied' boot log line"
  echo "$LOGS" | tail -30
  exit 1
fi
echo "[test-cold-start] ok: libsql migrations applied on first boot"

if [[ -z "$EXPECTED_SCHEMA_VERSION" ]]; then
  echo "[test-cold-start] FAIL: no zpay-store migrations found"
  exit 1
fi

if ! echo "$LOGS" | grep -q "\"schema_version\":$EXPECTED_SCHEMA_VERSION"; then
  echo "[test-cold-start] FAIL: schema_version != $EXPECTED_SCHEMA_VERSION"
  exit 1
fi
echo "[test-cold-start] ok: schema_version=$EXPECTED_SCHEMA_VERSION reached"

if ! echo "$LOGS" | grep -q '"message":"zpay-runtime ready"'; then
  echo "[test-cold-start] FAIL: missing 'zpay-runtime ready' boot log line"
  exit 1
fi
echo "[test-cold-start] ok: zpay-runtime ready signal emitted"

echo "[test-cold-start] round-tripping /prepare against freshly migrated database"
BODY="$(cat <<EOF
{
  "payee_id": "aether-demo",
  "network": "testnet",
  "scheme": "zcash",
  "resource_uri": "aether-demo/items/probe",
  "nonce": "cold-start-$$",
  "idempotency_key": "cold-start-$$"
}
EOF
)"

PROOF="$(python3 "$SCRIPT_DIR/mint-dpop-proof.py" \
  --keyfile "$DPOP_KEYFILE" \
  --method POST \
  --url "$URL/zpay/v1/prepare" \
  --jti "cold-start-jti-$$")"

RESPONSE="$(curl -fsS -X POST "$URL/zpay/v1/prepare" \
  -H 'content-type: application/json' \
  -H "DPoP: $PROOF" \
  -d "$BODY")"
PAYMENT_ID="$(echo "$RESPONSE" | python3 -c 'import sys,json;print(json.load(sys.stdin)["payment_id"])')"
PAYMENT_URI="$(echo "$RESPONSE" | python3 -c 'import sys,json;print(json.load(sys.stdin)["payment_uri"])')"

if [[ -z "$PAYMENT_ID" ]]; then
  echo "[test-cold-start] FAIL: /prepare returned empty payment_id"
  echo "  response: $RESPONSE"
  exit 1
fi
echo "[test-cold-start] ok: /prepare returned payment_id=$PAYMENT_ID"

if [[ "${PAYMENT_URI#zcash:}" == "$PAYMENT_URI" ]]; then
  echo "[test-cold-start] FAIL: payment_uri does not start with 'zcash:'"
  echo "  uri: $PAYMENT_URI"
  exit 1
fi
echo "[test-cold-start] ok: payment_uri starts with zcash:"

STATUS="$(curl -fsS "$URL/zpay/v1/payments/$PAYMENT_ID" | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')"
if [[ "$STATUS" != "awaiting" ]]; then
  echo "[test-cold-start] FAIL: GET /payments/{id} status is '$STATUS', expected 'awaiting'"
  exit 1
fi
echo "[test-cold-start] ok: GET /payments/{id} returns status=awaiting"

echo "[test-cold-start] PASS"
