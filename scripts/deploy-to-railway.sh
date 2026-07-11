#!/usr/bin/env bash
set -euo pipefail

# Deploy zpay's Railway services from per-service staged trees.
#
# Why staging: the three services (zpay, zspend, zpay-demo) share this
# one repository, but Railway applies any railway.toml found at the
# upload root to whichever service the upload targets, and always
# builds a ./Dockerfile it finds there. Each service therefore deploys
# from its own staged tree holding exactly one Dockerfile and one
# railway.toml, overlaid from Dockerfile.<service> and
# railway.<service>.toml.
#
# Why an allowlist: staging only what each image builds from keeps
# stray local files (payee overrides, wallet artifacts, .env files)
# out of the upload, and a future Dockerfile COPY of an unstaged path
# fails loudly at Railway's COPY step.
#
# Railway healthchecks probe the port carried in the PORT service
# variable, so each service must have PORT set to its app port; the
# preflight below asserts that and prints the remediation command.
#
# Usage:
#   ./scripts/deploy-to-railway.sh <zpay|zspend|zpay-demo|all> [--check] [--detach]

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVICES_ALL="zspend zpay zpay-demo"
CHECK_ONLY=0
DETACH_FLAG=""
SELECTED=""

usage() {
  cat <<'USAGE'
deploy-to-railway.sh - deploy zpay's Railway services from staged trees

Usage:
  ./scripts/deploy-to-railway.sh <zpay|zspend|zpay-demo|all> [flags]

Flags:
  --check    Stage and validate without pushing; prints the stage dir.
  --detach   Return immediately after upload; do not stream deploy logs.
  --help     Print this message.
USAGE
}

for arg in "$@"; do
  case "$arg" in
    zpay|zspend|zpay-demo|all)
      if [[ -n "$SELECTED" ]]; then
        echo "deploy-to-railway.sh: multiple service arguments ($SELECTED, $arg); pass one service or 'all'." >&2
        exit 64
      fi
      SELECTED="$arg"
      ;;
    --check)
      CHECK_ONLY=1
      ;;
    --detach)
      DETACH_FLAG="--detach"
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "deploy-to-railway.sh: unknown argument $arg" >&2
      usage >&2
      exit 64
      ;;
  esac
done

if [[ -z "$SELECTED" ]]; then
  echo "deploy-to-railway.sh: a service argument is required." >&2
  usage >&2
  exit 64
fi

service_dockerfile() {
  case "$1" in
    zpay) echo "Dockerfile" ;;
    zspend) echo "Dockerfile.zspend" ;;
    zpay-demo) echo "Dockerfile.zpay-demo" ;;
  esac
}

service_port() {
  case "$1" in
    zpay) echo "8080" ;;
    zspend) echo "8090" ;;
    zpay-demo) echo "7410" ;;
  esac
}

service_healthcheck_path() {
  case "$1" in
    zpay|zspend) echo "/healthz" ;;
    zpay-demo) echo "/demo/v1/readiness" ;;
  esac
}

if ! command -v railway >/dev/null 2>&1; then
  echo "deploy-to-railway.sh: railway CLI not found in PATH." >&2
  echo "Install: https://docs.railway.com/guides/cli" >&2
  exit 127
fi

if [[ "$CHECK_ONLY" -eq 0 ]] && ! railway whoami >/dev/null 2>&1; then
  echo "deploy-to-railway.sh: railway is not authenticated (railway whoami failed)." >&2
  echo "Run: railway login" >&2
  exit 1
fi

if ! python3 -c 'import tomllib' >/dev/null 2>&1; then
  echo "deploy-to-railway.sh: python3 with tomllib (3.11+) is required for config validation." >&2
  echo "Found: $(python3 --version 2>&1)" >&2
  exit 1
fi

STAGE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/zpay-railway-XXXXXX")"
if [[ "$CHECK_ONLY" -eq 0 ]]; then
  trap 'rm -rf "$STAGE_ROOT"' EXIT
fi

stage_service() {
  local service="$1"
  local stage_dir="$STAGE_ROOT/$service"
  mkdir -p "$stage_dir"

  cp "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock" "$stage_dir/"
  rsync -a --exclude='target' "$REPO_ROOT/crates/" "$stage_dir/crates/"
  rsync -a "$REPO_ROOT/docker/" "$stage_dir/docker/"

  case "$service" in
    zpay)
      mkdir -p "$stage_dir/etc"
      cp "$REPO_ROOT/etc/aether-demo.toml" "$stage_dir/etc/"
      ;;
    zpay-demo)
      rsync -a \
        --exclude='node_modules' \
        --exclude='dist' \
        --exclude='test-results' \
        --exclude='playwright-report' \
        --exclude='coverage' \
        --exclude='.tmp' \
        "$REPO_ROOT/demo/" "$stage_dir/demo/"
      ;;
  esac

  cp "$REPO_ROOT/$(service_dockerfile "$service")" "$stage_dir/Dockerfile"
  cp "$REPO_ROOT/railway.$service.toml" "$stage_dir/railway.toml"

  local strays
  strays="$(cd "$stage_dir" && ls Dockerfile.* railway.*.toml 2>/dev/null || true)"
  if [[ -n "$strays" ]]; then
    echo "stage_service($service): unexpected config strays in stage: $strays" >&2
    exit 1
  fi

  echo "$stage_dir"
}

validate_stage() {
  local service="$1"
  local stage_dir="$2"
  local expected_healthcheck
  expected_healthcheck="$(service_healthcheck_path "$service")"

  SERVICE_NAME="$service" STAGE_DIR="$stage_dir" EXPECTED_HEALTHCHECK="$expected_healthcheck" \
    python3 - <<'PY'
import os
import sys
import tomllib

stage_dir = os.environ["STAGE_DIR"]
service = os.environ["SERVICE_NAME"]
expected_healthcheck = os.environ["EXPECTED_HEALTHCHECK"]

with open(os.path.join(stage_dir, "railway.toml"), "rb") as config_file:
    config = tomllib.load(config_file)

build = config.get("build", {})
deploy = config.get("deploy", {})
problems = []
if build.get("builder") != "DOCKERFILE":
    problems.append(f"build.builder is {build.get('builder')!r}, expected 'DOCKERFILE'")
if build.get("dockerfilePath") != "Dockerfile":
    problems.append(f"build.dockerfilePath is {build.get('dockerfilePath')!r}, expected 'Dockerfile'")
if deploy.get("healthcheckPath") != expected_healthcheck:
    problems.append(
        f"deploy.healthcheckPath is {deploy.get('healthcheckPath')!r}, expected {expected_healthcheck!r}"
    )
if not deploy.get("restartPolicyType"):
    problems.append("deploy.restartPolicyType is missing")
if not os.path.isfile(os.path.join(stage_dir, "Dockerfile")):
    problems.append("staged Dockerfile is missing")

if problems:
    print(f"validate_stage({service}): " + "; ".join(problems), file=sys.stderr)
    sys.exit(1)
print(f"validate_stage({service}): railway.toml OK (healthcheck {expected_healthcheck})")
PY
}

assert_port_variable() {
  local service="$1"
  local expected_port variable_listing
  expected_port="$(service_port "$service")"
  if ! variable_listing="$(railway variable list --service "$service" --kv)"; then
    echo "assert_port_variable($service): railway variable list failed; see the CLI error above." >&2
    exit 1
  fi
  if ! grep -q "^PORT=${expected_port}$" <<<"$variable_listing"; then
    echo "assert_port_variable($service): PORT=${expected_port} is not set on the service." >&2
    echo "Railway healthchecks probe the PORT variable; without it the deploy fails its healthcheck." >&2
    echo "Fix with: railway variable set PORT=${expected_port} --service ${service} --skip-deploys" >&2
    exit 1
  fi
}

deploy_service() {
  local service="$1"
  local stage_dir
  stage_dir="$(stage_service "$service")"
  validate_stage "$service" "$stage_dir"

  if [[ "$CHECK_ONLY" -eq 1 ]]; then
    echo "--check: skipping railway up for $service. Stage dir: $stage_dir"
    return 0
  fi

  local sha message
  sha="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  message="deploy $service from $sha"
  if [[ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]]; then
    message="$message+dirty"
  fi

  # CWD stays at the repo root: the CLI indexes the current directory's
  # surroundings, and a CWD under /private/tmp trips on stray sockets.
  (
    cd "$REPO_ROOT"
    railway up "$stage_dir" \
      --path-as-root \
      --service "$service" \
      --message "$message" \
      $DETACH_FLAG \
      --verbose
  )
}

if [[ "$SELECTED" == "all" ]]; then
  SERVICES_SELECTED="$SERVICES_ALL"
else
  SERVICES_SELECTED="$SELECTED"
fi

# Preflight every selected service before the first upload so a failed
# assertion cannot leave the stack version-skewed mid-run.
if [[ "$CHECK_ONLY" -eq 0 ]]; then
  for service in $SERVICES_SELECTED; do
    assert_port_variable "$service"
  done
fi

for service in $SERVICES_SELECTED; do
  deploy_service "$service"
done
