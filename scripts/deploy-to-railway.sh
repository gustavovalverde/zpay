#!/usr/bin/env bash
set -euo pipefail

# Deploy zpay-runtime to Railway from a clean staged tree.
#
# Why staging: railway up's HTTP upload times out on multi-hundred-MB
# tarballs, and the CLI honors .gitignore (not .dockerignore). We rsync
# only what the Dockerfile needs into a temp dir before uploading.
#
# Why no workspace siblings: zpay is a self-contained Cargo workspace,
# so this script is a thin wrapper around `railway up --service zpay`
# rather than the cross-workspace staging dance the zentity web service
# needs. The rsync still removes target/ and .git/ to keep the upload
# small.
#
# Usage:
#   ./scripts/deploy-to-railway.sh           # interactive (railway picks up project link)
#   ./scripts/deploy-to-railway.sh --detach  # do not stream logs after upload
#   ./scripts/deploy-to-railway.sh --check   # stage + validate, do not push (CI-friendly dry run)

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE_DIR="${TMPDIR:-/tmp}/zpay-railway-deploy"
SERVICE="zpay"
DETACH_FLAG=""
CHECK_ONLY=0

for arg in "$@"; do
  case "$arg" in
    --detach)
      DETACH_FLAG="--detach"
      ;;
    --check)
      CHECK_ONLY=1
      ;;
    --help|-h)
      cat <<'USAGE'
deploy-to-railway.sh - upload zpay-runtime to Railway

Flags:
  --detach   Return immediately after upload; do not stream deploy logs.
  --check    Stage the upload and validate railway.toml without pushing.
  --help     Print this message.
USAGE
      exit 0
      ;;
    *)
      echo "deploy-to-railway.sh: unknown flag $arg" >&2
      echo "Run with --help for usage." >&2
      exit 64
      ;;
  esac
done

if ! command -v railway >/dev/null 2>&1; then
  echo "deploy-to-railway.sh: railway CLI not found in PATH." >&2
  echo "Install: https://docs.railway.com/guides/cli" >&2
  exit 127
fi

# Soft auth check; the CLI itself enforces this on upload, but warning
# early avoids a long rsync followed by a denied push.
if ! railway whoami >/dev/null 2>&1; then
  echo "deploy-to-railway.sh: railway is not authenticated (railway whoami failed)." >&2
  echo "Run: railway login" >&2
  if [[ "$CHECK_ONLY" -eq 0 ]]; then
    exit 1
  fi
fi

echo "Staging deploy tree at $STAGE_DIR ..."
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cd "$REPO_ROOT"

rsync -a \
  --exclude='.git' \
  --exclude='target' \
  --exclude='node_modules' \
  --exclude='*.log' \
  --exclude='.DS_Store' \
  --exclude='.vscode' \
  --exclude='.idea' \
  --exclude='scripts/.tmp' \
  ./ "$STAGE_DIR/"

echo "Stage tree size:"
du -sh "$STAGE_DIR"

cd "$STAGE_DIR"

# Validate the railway.toml at the upload root. The CLI does not expose
# a "validate" verb today, so we parse with python's tomllib (stdlib in
# 3.11+) as a syntax gate; the schema itself is enforced by Railway on
# upload.
if command -v python3 >/dev/null 2>&1; then
  python3 - <<'PY'
import sys
import tomllib
with open("railway.toml", "rb") as f:
    data = tomllib.load(f)
build = data.get("build", {})
deploy = data.get("deploy", {})
required = [
    ("build.builder", build.get("builder")),
    ("deploy.healthcheckPath", deploy.get("healthcheckPath")),
    ("deploy.restartPolicyType", deploy.get("restartPolicyType")),
]
missing = [name for name, value in required if not value]
if missing:
    print(f"railway.toml missing required keys: {missing}", file=sys.stderr)
    sys.exit(1)
print("railway.toml parses OK ({} keys)".format(len(data)))
PY
fi

if [[ "$CHECK_ONLY" -eq 1 ]]; then
  echo "--check: skipping railway up. Stage dir: $STAGE_DIR"
  exit 0
fi

echo "Running railway up --service $SERVICE ..."
railway up \
  --service "$SERVICE" \
  --message "deploy from $(git -C "$REPO_ROOT" rev-parse --short HEAD)" \
  $DETACH_FLAG \
  --verbose
