#!/bin/sh
set -e

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zspend` user. Mirrors the gosu pattern used by the zpay
# image and apps/fhe in the zentity repo.
if [ "$(id -u)" = "0" ]; then
    chown -R zspend:zspend /var/lib/zspend 2>/dev/null || true
    exec gosu zspend /app/zspend-runtime
else
    exec /app/zspend-runtime
fi
