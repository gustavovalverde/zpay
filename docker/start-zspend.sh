#!/bin/sh
set -e

# Fix bind-mount permissions when started as root, then drop to the
# non-root `zspend` user. Mirrors the gosu pattern used by the zpay
# image and apps/fhe in the zentity repo.
#
# The sealed seed at $ZSPEND_SEALED_SEED_PATH is the only backup of the
# wallet. The entrypoint never auto-provisions one unless
# $ZSPEND_ALLOW_AUTO_PROVISION=1 is set (a throwaway dev wallet); otherwise
# it refuses to boot so an operator provisions the seed explicitly and
# stores the revealed mnemonic offline.

ensure_seed() {
    # $1: command prefix used to run init as the target user (e.g. "gosu zspend").
    run_as="$1"
    if [ -z "${ZSPEND_SEALED_SEED_PATH:-}" ]; then
        echo "start-zspend: ZSPEND_SEALED_SEED_PATH is unset; cannot locate the sealed seed." >&2
        exit 1
    fi
    if [ -e "${ZSPEND_SEALED_SEED_PATH}" ]; then
        return 0
    fi
    if [ "${ZSPEND_ALLOW_AUTO_PROVISION:-}" = "1" ]; then
        echo "start-zspend: no sealed seed at ${ZSPEND_SEALED_SEED_PATH}; ZSPEND_ALLOW_AUTO_PROVISION=1, provisioning a throwaway dev wallet." >&2
        # --auto-provision seals the seed without printing the mnemonic: this is
        # an unbacked dev wallet, and the phrase must never reach the logs.
        $run_as /app/zspend-runtime init --auto-provision
        return 0
    fi
    echo "start-zspend: no sealed seed at ${ZSPEND_SEALED_SEED_PATH} and ZSPEND_ALLOW_AUTO_PROVISION is not set; refusing to auto-provision." >&2
    echo "start-zspend: provision the wallet explicitly, then restart:" >&2
    echo "start-zspend:   zspend-runtime init            # generate a new sealed seed and reveal its mnemonic once" >&2
    echo "start-zspend:   zspend-runtime init --restore  # seal a seed from a mnemonic supplied on stdin" >&2
    echo "start-zspend: or set ZSPEND_ALLOW_AUTO_PROVISION=1 to allow a throwaway dev wallet." >&2
    exit 1
}

if [ "$(id -u)" = "0" ]; then
    chown -R zspend:zspend /var/lib/zspend 2>/dev/null || true
    ensure_seed "gosu zspend"
    exec gosu zspend /app/zspend-runtime serve
else
    ensure_seed ""
    exec /app/zspend-runtime serve
fi
