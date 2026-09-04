# syntax=docker/dockerfile:1

# summ — an OCI Distribution Spec registry in one binary.
#
# Multi-stage: a builder on the official Rust image, a runtime on
# debian-slim holding nothing but the binary and the shared libraries
# RocksDB's C++ needs. The dependency build is cached as its own layer with
# cargo-chef, which matters more here than in most Rust projects:
# librocksdb-sys compiles RocksDB 11.8 from source, so a cold build is
# minutes of C++ and an uncached one repeats it on every source edit.

ARG RUST_VERSION=1.93
ARG DEBIAN_VERSION=bookworm

# ---------------------------------------------------------------- toolchain --

FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS chef
WORKDIR /build

# librocksdb-sys compiles RocksDB from source and runs bindgen over its
# headers, so it needs a clang for libclang. lld cuts the link step, which is
# otherwise the slowest part of an incremental build. Same list as CI.
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang lld \
 && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked --version ^0.1

# ------------------------------------------------------------------- recipe --

# The plan is a description of the dependency graph and nothing else, so it is
# unchanged by an edit to our own sources — which is what keeps the cook layer
# below valid across ordinary commits.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# -------------------------------------------------------------------- build --

FROM chef AS builder

# Dependencies first, as their own layer. Only Cargo.lock and the manifests
# feed this, so RocksDB is rebuilt when a dependency moves and never because a
# handler did.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release --locked --bin summ \
 && strip target/release/summ

# ------------------------------------------------------------------ runtime --

FROM debian:${DEBIAN_VERSION}-slim AS runtime

# RocksDB is statically linked, so the only dynamic dependencies left are the
# ones the OS is expected to provide: libstdc++ for the C++ runtime (not in
# the slim base), and curl purely for the HEALTHCHECK below.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libstdc++6 curl \
 && rm -rf /var/lib/apt/lists/*

# An unprivileged fixed uid, so a bind-mounted host directory can be chowned
# to a number that does not move between builds.
RUN groupadd --system --gid 10001 summ \
 && useradd --system --uid 10001 --gid summ --home-dir /var/lib/summ --shell /usr/sbin/nologin summ \
 && mkdir -p /var/lib/summ \
 && chown summ:summ /var/lib/summ

COPY --from=builder /build/target/release/summ /usr/local/bin/summ

# `meta/` and `blobs/` live here and must share a filesystem — an upload is
# committed by renaming its staging file into the blob tree, and a rename
# across devices is not a rename. So this is one volume, never two.
ENV SUMM_DATA_DIR=/var/lib/summ \
    SUMM_LOG=summ=info,summ_server=info,tower_http=info
VOLUME ["/var/lib/summ"]

# The container always serves on 5000, and that is not configurable here. A
# host wanting a different number publishes one — `-p 8080:5000` — because the
# container has its own network namespace, so the port inside cannot collide
# with anything and there is nothing for a knob to solve. Fixing it keeps
# EXPOSE, the healthcheck and every compose file or `containerPort` in
# agreement about a single number.
#
# `0.0.0.0`, not clap's `127.0.0.1` default: a published port forwards to the
# container's external interface, which a loopback listener never sees.
ENV SUMM_LISTEN=0.0.0.0:5000
EXPOSE 5000

USER summ:summ

# `GET /v2/` is the spec's own liveness probe: it answers 200 with an empty
# object and touches neither the blob store nor a scan. 5000 is a constant
# here for the same reason it is above: the container's port does not move.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:5000/v2/ || exit 1

# Split so that `docker run <image> serve --engine redb` replaces the
# arguments and keeps the binary. The server already stops on SIGTERM and
# drains in-flight requests, and exec form is what lets the signal reach it.
ENTRYPOINT ["/usr/local/bin/summ"]
CMD ["serve"]
