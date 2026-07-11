#!/bin/sh
set -e

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zpay-demo` user. Mirrors the gosu pattern used by the zpay
# and zspend images.

if [ -n "${ZPAY_DEMO_ISSUER_KEY_PEM:-}" ] && [ -n "${ZPAY_DEMO_ISSUER_KEY_PATH:-}" ]; then
    mkdir -p "$(dirname "$ZPAY_DEMO_ISSUER_KEY_PATH")"
    printf '%s' "$ZPAY_DEMO_ISSUER_KEY_PEM" > "$ZPAY_DEMO_ISSUER_KEY_PATH"
fi

if [ "$(id -u)" = "0" ]; then
    chown -R zpay-demo:zpay-demo /var/lib/zpay-demo 2>/dev/null || true
    exec gosu zpay-demo /app/zpay-demo
else
    exec /app/zpay-demo
fi
