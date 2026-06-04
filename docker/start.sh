#!/bin/sh
set -e

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zpay` user. Matches the gosu pattern used by apps/fhe in
# the zentity repo.
if [ "$(id -u)" = "0" ]; then
    chown -R zpay:zpay /var/lib/zpay 2>/dev/null || true
    exec gosu zpay /app/zpay-runtime
else
    exec /app/zpay-runtime
fi
