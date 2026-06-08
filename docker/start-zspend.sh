#!/bin/sh
set -e

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zspend` user. Mirrors the gosu pattern used by the zpay
# image and apps/fhe in the zentity repo.
#
# If the sealed seed at $ZSPEND_SEALED_SEED_PATH is missing (first boot
# on a fresh volume), provision it via `zspend-runtime init` before the
# serve subcommand opens it. The init step honors $ZSPEND_SEALED_SEED_PATH
# via clap's `env` attribute so the binary writes to the same path serve
# will read.
run_init_if_missing() {
    if [ -z "${ZSPEND_SEALED_SEED_PATH:-}" ]; then
        echo "start-zspend: ZSPEND_SEALED_SEED_PATH is unset, skipping init probe" >&2
        return 0
    fi
    if [ ! -e "${ZSPEND_SEALED_SEED_PATH}" ]; then
        echo "start-zspend: sealed seed not found at ${ZSPEND_SEALED_SEED_PATH}, running init" >&2
        /app/zspend-runtime init
    fi
}

if [ "$(id -u)" = "0" ]; then
    chown -R zspend:zspend /var/lib/zspend 2>/dev/null || true
    gosu zspend /bin/sh -c '
        set -e
        if [ -n "${ZSPEND_SEALED_SEED_PATH:-}" ] && [ ! -e "${ZSPEND_SEALED_SEED_PATH}" ]; then
            echo "start-zspend: sealed seed not found at ${ZSPEND_SEALED_SEED_PATH}, running init" >&2
            /app/zspend-runtime init
        fi
    '
    exec gosu zspend /app/zspend-runtime serve
else
    run_init_if_missing
    exec /app/zspend-runtime serve
fi
