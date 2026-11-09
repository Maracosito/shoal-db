# syntax=docker/dockerfile:1.6
# Shoal server — build from milestone-85 source + milestone-86 deploy
#
# Build:  docker build -f deploy/docker/Dockerfile -t shoal/shoal:dev .

ARG RUST_IMAGE=rust:1-bookworm

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY Cargo.toml .
COPY crates crates

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --release --workspace && \
    mkdir -p /out && \
    cp target/release/shoal-server /out/ && \
    # Create shoal-worker as symlink to shoal-server (same binary, different entrypoint)
    cp target/release/shoal-server /out/shoal-worker

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

ENTRYPOINT ["shoal-server"]
