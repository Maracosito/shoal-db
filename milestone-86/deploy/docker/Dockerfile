# syntax=docker/dockerfile:1.6
#
# Multi-stage build for the Shoal server, background worker, and CLI.
#
# Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t shoal/shoal:dev .
#
# The resulting image contains three binaries:
#   shoal-server  - the HTTP API / query-serving node (default entrypoint)
#   shoal-worker  - the background indexer / compaction worker
#   shoal         - the CLI (handy for debugging inside the container)

ARG RUST_IMAGE=rust:1-bookworm

FROM ${RUST_IMAGE} AS builder
WORKDIR /src

# Copy the full workspace. The repo's .dockerignore should exclude target/,
# node_modules/, and other build artifacts.
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --release --workspace && \
    mkdir -p /out && \
    cp target/release/shoal-server /out/ && \
    cp target/release/shoal-worker /out/ && \
    if [ -f target/release/shoal ]; then cp target/release/shoal /out/; fi

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --create-home --home-dir /home/shoal shoal && \
    mkdir -p /var/cache/shoal && \
    chown -R shoal:shoal /var/cache/shoal

COPY --from=builder /out/ /usr/local/bin/

ENV SHOAL_LISTEN_ADDR=0.0.0.0:8080 \
    SHOAL_CACHE_DIR=/var/cache/shoal \
    SHOAL_LOG=info

USER shoal
EXPOSE 8080

# Default to the API server; the worker overrides this with `command: [shoal-worker]`.
ENTRYPOINT ["shoal-server"]
