# Shoal Docker deployment

This directory contains the Shoal container image definition and a Docker
Compose stack for local development and evaluation.

## Quick start

From the repository root:

```sh
docker compose -f deploy/docker/docker-compose.yml up --build
```

Once everything is healthy:

| Endpoint | URL | Credentials |
|---|---|---|
| Shoal API | http://localhost:8080 | API key `dev-root-key` |
| Example search app | http://localhost:8000 | — |
| MinIO console | http://localhost:9001 | `shoaladmin` / `shoalsecret` |

The example app seeds a `demo-articles` namespace on startup and serves a
small web UI demonstrating vector, full-text, and hybrid search through the
Python SDK.

## Image layout

`Dockerfile` is a multi-stage build producing a slim Debian runtime image
with three binaries: `shoal-server` (default entrypoint), `shoal-worker`,
and the `shoal` CLI. Build it standalone with:

```sh
docker build -f deploy/docker/Dockerfile -t shoal/shoal:dev .
```

## Configuration

All configuration is via environment variables. See
[`docs/deployment.md`](../../docs/deployment.md) for the full reference,
including cache sizing, S3 settings, and production hardening guidance.

The credentials in `docker-compose.yml` (`dev-root-key`, `shoaladmin` /
`shoalsecret`) are for local development only — never reuse them in a
deployed environment.
