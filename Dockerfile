# syntax=docker/dockerfile:1

# zpay-runtime: x402 Zcash facilitator
#
# Multi-stage Rust workspace build, mirrors apps/fhe/Dockerfile from
# the zentity repo so operators who know that pattern get the same
# shape here: cargo cache mounts in the builder stage, non-root user
# in the runtime stage, gosu shim to fix bind-mount permissions, and
# a curl-based healthcheck.

ARG SOURCE_DATE_EPOCH=0
ARG GIT_SHA=unknown
ARG BUILD_TIME=unknown

ARG APP_UID=10001
ARG APP_GID=${APP_UID}
ARG APP_USER=zpay
ARG APP_HOME=/app

# --- Stage 1: Builder ---
FROM rust:1.95-trixie AS builder
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG APP_HOME
WORKDIR ${APP_HOME}

ARG SOURCE_DATE_EPOCH
ARG GIT_SHA
ARG BUILD_TIME
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/* /tmp/*

# Copy the full workspace. The workspace has seven crates with
# interlinked path deps; the dummy-src cache trick used in single-
# crate Dockerfiles does not extend cleanly here.
COPY --link Cargo.toml Cargo.lock ./
COPY --link crates ./crates

ENV GIT_SHA=${GIT_SHA}
ENV BUILD_TIME=${BUILD_TIME}
RUN cargo build --locked --release --bin zpay-runtime && \
    cp ${APP_HOME}/target/release/zpay-runtime ${APP_HOME}/zpay-runtime

# --- Stage 2: Runtime ---
# Trixie-slim matches the rust:1.95-trixie builder so the binary's
# glibc + libstdc++ symbol requirements (e.g. GLIBC_2.38,
# GLIBCXX_3.4.31) are satisfied. Mixing a trixie-built binary with a
# bookworm-slim runtime fails with dynamic-linker version errors.
FROM debian:trixie-slim AS runtime

ARG APP_UID
ARG APP_GID
ARG APP_USER
ARG APP_HOME

WORKDIR ${APP_HOME}

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gosu \
    && rm -rf /var/lib/apt/lists/* /tmp/*

COPY --link --from=builder ${APP_HOME}/zpay-runtime ${APP_HOME}/zpay-runtime
COPY --link etc/aether-demo.toml /etc/zpay/payees.toml
COPY --link docker/start.sh ./start.sh

RUN groupadd --gid ${APP_GID} ${APP_USER} && \
    useradd --uid ${APP_UID} --gid ${APP_USER} --shell /sbin/nologin ${APP_USER} && \
    mkdir -p /var/lib/zpay /opt/zpay-home/.zcash-params /home/zpay && \
    ln -s /opt/zpay-home/.zcash-params /home/zpay/.zcash-params && \
    touch /var/lib/zpay/.keep && \
    chmod +x start.sh && \
    chown -R ${APP_UID}:${APP_GID} ${APP_HOME} /var/lib/zpay /opt/zpay-home /home/zpay

# Sapling trusted-setup parameters: the same files zcashd/lightwalletd fetch
# via fetch-params.sh. Baked into the image so PCZT verification/extraction
# needs no outbound fetch at boot (see docs/runbooks/railway-deploy.md).
RUN curl -fsSL -o /opt/zpay-home/.zcash-params/sapling-spend.params \
        https://download.z.cash/downloads/sapling-spend.params && \
    curl -fsSL -o /opt/zpay-home/.zcash-params/sapling-output.params \
        https://download.z.cash/downloads/sapling-output.params && \
    chown -R ${APP_UID}:${APP_GID} /opt/zpay-home/.zcash-params

ENV RUST_LOG=info \
    HOME=/opt/zpay-home \
    ZPAY_SERVER__BIND_ADDR=0.0.0.0:8080 \
    ZPAY_OPS__BIND_ADDR=0.0.0.0:9295 \
    ZPAY_NETWORK=testnet \
    ZPAY_STORE__BACKEND=libsql \
    ZPAY_STORE__URL=file:/var/lib/zpay/zpay.libsql \
    ZPAY_PAYEES__CONFIG_PATH=/etc/zpay/payees.toml

EXPOSE 8080 9295

# Liveness probe: `/healthz` returns 200 + {"status":"alive"} on a
# healthy listener. Dependency-agnostic by design: this signal tells
# the orchestrator the process answers, not that downstream services
# are reachable. Use the WARN posture logs for downstream alerts.
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD curl -fsS --max-time 5 -o /dev/null "http://localhost:8080/healthz" || exit 1

CMD ["./start.sh"]
