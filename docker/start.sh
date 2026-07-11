#!/bin/sh
set -e

# Escape hatch for operators who want to ship a payee registry without
# baking a custom image (e.g. Railway deploys): if set, this overwrites
# the file at $ZPAY_PAYEES__CONFIG_PATH on every start.
if [ -n "${ZPAY_PAYEES_TOML:-}" ]; then
    printf '%s' "$ZPAY_PAYEES_TOML" > "${ZPAY_PAYEES__CONFIG_PATH:-/etc/zpay/payees.toml}"
fi

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zpay` user. Matches the gosu pattern used by apps/fhe in
# the zentity repo.
if [ "$(id -u)" = "0" ]; then
    chown -R zpay:zpay /var/lib/zpay 2>/dev/null || true
    exec gosu zpay /app/zpay-runtime
else
    exec /app/zpay-runtime
fi
