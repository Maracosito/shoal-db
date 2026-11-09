# ---- build stage -------------------------------------------------------------
FROM rust:1.78-slim AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config build-essential \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

RUN cargo build --release -p gannet-server

# ---- runtime stage -----------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin gannet

COPY --from=builder /app/target/release/gannet-server /usr/local/bin/gannet-server

USER gannet
EXPOSE 8080

ENTRYPOINT ["gannet-server"]
