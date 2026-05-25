# syntax=docker/dockerfile:1.7

# zpay-runtime production image.
#
# Multi-stage: build stage compiles the binary under cargo's BuildKit
# cache mounts; runtime stage carries only the binary plus runtime
# dependencies.

ARG RUST_VERSION=1.95.0
ARG DEBIAN_VARIANT=bookworm-slim

FROM rust:${RUST_VERSION} AS builder
WORKDIR /usr/src/zpay

# Toolchain-pinning files (matches rust-toolchain.toml in the repo root).
COPY rust-toolchain.toml ./
COPY rustfmt.toml ./
COPY clippy.toml ./
COPY Cargo.toml ./

# Crates and migrations.
COPY crates ./crates

# Build the runtime binary under cargo's BuildKit cache mounts so re-runs
# during local iteration do not redownload dependencies.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/zpay/target \
    cargo build --release --bin zpay-runtime \
 && cp /usr/src/zpay/target/release/zpay-runtime /usr/local/bin/zpay-runtime

FROM debian:${DEBIAN_VARIANT} AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/*

# Non-root user; Railway uses the same convention across our Rust services.
RUN useradd --system --create-home --uid 10001 zpay
USER zpay
WORKDIR /home/zpay

COPY --from=builder /usr/local/bin/zpay-runtime /usr/local/bin/zpay-runtime

ENV ZPAY_SERVER__BIND_ADDR=0.0.0.0:8080
ENV ZPAY_OPS__BIND_ADDR=0.0.0.0:9295

EXPOSE 8080 9295

ENTRYPOINT ["/usr/local/bin/zpay-runtime"]
