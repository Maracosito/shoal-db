# Shoal deployment guide

This guide covers running Shoal locally with Docker Compose, deploying to
Kubernetes (raw manifests or Helm), configuration reference, and production
hardening.

Shoal's architecture makes deployment unusually simple: **all durable state
lives in an S3-compatible object store**. Compute nodes (API servers and
workers) are stateless apart from rebuildable local caches, so they can be
replaced, scaled, or rescheduled freely.

## Components

| Component | Binary | Role |
|---|---|---|
| API server | `shoal-server` | HTTP API, query serving, durable WAL writes, cache hierarchy |
| Worker | `shoal-worker` | Background indexing, compaction, segment building |
| Object store | any S3-compatible | Durable source of truth (WAL, segments, manifests, branch refs) |

## 1. Local development: Docker Compose

From the repository root:

```sh
docker compose -f deploy/docker/docker-compose.yml up --build
```

This starts MinIO (with bucket auto-creation), a Shoal API server, a
background worker, and an example search app:

| Endpoint | URL | Credentials |
|---|---|---|
| Shoal API | http://localhost:8080 | API key `dev-root-key` |
| Example app | http://localhost:8000 | — |
| MinIO console | http://localhost:9001 | `shoaladmin` / `shoalsecret` |

Smoke test:

```sh
curl -H "Authorization: Bearer dev-root-key" http://localhost:8080/healthz
```

All compose credentials are development-only. State persists in named
volumes; `docker compose down -v` wipes everything.

## 2. Configuration reference

All configuration is via environment variables, identical across Compose,
Kubernetes, and bare-metal deployments.

| Variable | Default | Description |
|---|---|---|
| `SHOAL_LISTEN_ADDR` | `0.0.0.0:8080` | API server bind address |
| `SHOAL_STORAGE_BACKEND` | — | `s3` or `fs` |
| `SHOAL_S3_ENDPOINT` | (empty = AWS) | Custom S3 endpoint, e.g. `http://minio:9000` |
| `SHOAL_S3_BUCKET` | — | Bucket for all Shoal state |
| `SHOAL_S3_REGION` | `us-east-1` | Bucket region |
| `SHOAL_S3_FORCE_PATH_STYLE` | `false` | Required `true` for MinIO and most non-AWS stores |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | — | Static credentials; omit when using IAM roles / IRSA |
| `SHOAL_FS_ROOT` | — | Root directory (only for `fs` backend; tests/dev) |
| `SHOAL_CACHE_DIR` | `/var/cache/shoal` | Local disk cache directory |
| `SHOAL_CACHE_DISK_BYTES` | backend default | Disk cache budget (LRU-evicted) |
| `SHOAL_CACHE_MEMORY_BYTES` | backend default | Memory cache budget for hot metadata/index blocks |
| `SHOAL_ROOT_API_KEY` | — | Bootstrap admin API key |
| `SHOAL_LOG` | `info` | Log level: `error`, `warn`, `info`, `debug`, `trace` |

Notes:

- Keep `SHOAL_CACHE_DISK_BYTES` ~20% below the actual volume size so the
  evictor has headroom.
- Metrics are exposed at `GET /metrics` (Prometheus text format) on the API
  port; health at `GET /healthz`.

## 3. Kubernetes: raw manifests

See [`deploy/k8s/README.md`](../deploy/k8s/README.md). Summary:

```sh
cp deploy/k8s/10-secret.example.yaml deploy/k8s/10-secret.yaml
$EDITOR deploy/k8s/10-secret.yaml deploy/k8s/20-configmap.yaml
kubectl apply -f deploy/k8s/00-namespace.yaml -f deploy/k8s/10-secret.yaml \
  -f deploy/k8s/20-configmap.yaml -f deploy/k8s/30-api.yaml -f deploy/k8s/40-worker.yaml
```

## 4. Kubernetes: Helm chart

```sh
helm install shoal deploy/helm/shoal \
  --namespace shoal --create-namespace \
  --set config.s3.bucket=my-shoal-bucket \
  --set config.s3.region=eu-west-1 \
  --set auth.rootApiKey="$(openssl rand -hex 32)"
```

With an externally managed secret (recommended):

```sh
kubectl -n shoal create secret generic shoal-credentials \
  --from-literal=SHOAL_ROOT_API_KEY=... \
  --from-literal=AWS_ACCESS_KEY_ID=... \
  --from-literal=AWS_SECRET_ACCESS_KEY=...

helm install shoal deploy/helm/shoal -n shoal \
  --set config.s3.bucket=my-shoal-bucket \
  --set auth.existingSecret=shoal-credentials
```

Key values (`deploy/helm/shoal/values.yaml` for all):

| Value | Purpose |
|---|---|
| `api.replicas` | Query-serving fan-out; scale freely |
| `worker.replicas` | Keep at **1** unless you understand worker coordination (below) |
| `config.cache.diskBytes` + `api.cacheVolume.sizeLimit` | Cache sizing (env budget must stay below volume limit) |
| `auth.existingSecret` | Bring-your-own credentials Secret |
| `ingress.*` | Optional ingress for the API |
| `serviceAccount.annotations` | Attach IRSA / workload-identity roles for keyless S3 access |

## 5. Scaling model and consistency

- **API servers scale horizontally.** Any replica can serve any namespace;
  the first query on a cold replica pays object-storage latency while the
  cache warms. Use the warm-cache endpoint or namespace pinning to pre-warm
  replicas after scale-up.
- **Read-your-writes is per-deployment as documented in
  `docs/architecture.md`.** Writes are durable in object storage on
  acknowledgement. In multi-replica deployments, a replica that has not yet
  refreshed a namespace manifest may briefly serve a slightly stale view;
  see the architecture guide's consistency section for the exact window and
  knobs.
- **Workers:** indexing and compaction tasks are claimed through
  object-storage manifests with conditional writes, but a single worker
  replica is the simplest, safest configuration and is sufficient for most
  workloads. Scale workers only if indexing lag (see the
  `shoal_indexing_lag_seconds` metric) grows under sustained ingest.

## 6. Production hardening checklist

**Credentials & auth**

- Generate a long random `SHOAL_ROOT_API_KEY`; use it only to mint scoped
  keys (admin/writer/reader), then keep it offline.
- Prefer IAM roles (IRSA, workload identity) over static S3 keys.
- Terminate TLS at your ingress/load balancer; the API speaks plain HTTP.

**Object storage**

- Enable bucket-level encryption at rest (SSE-S3 or SSE-KMS). Shoal is
  agnostic to provider-side encryption.
- Enable bucket versioning if you want an extra safety net against
  accidental deletion; Shoal's files are immutable-by-design, so versioning
  overhead is low.
- Block public access on the bucket. Shoal needs `GetObject`, `PutObject`,
  `DeleteObject`, and `ListBucket` on its prefix only.

**Resources & caches**

- Size memory limits at least 2× `SHOAL_CACHE_MEMORY_BYTES` to leave room
  for query execution.
- Use local NVMe-backed node storage for the cache volume where possible;
  cold-vs-warm latency is dominated by this tier.
- Pin business-critical namespaces (`POST /v1/namespaces/{ns}/pin`) so they
  survive cache pressure.

**Observability**

- Scrape `/metrics`; alert on indexing lag, compaction backlog, object-store
  error rates, and cache hit ratio regressions.
- Logs are structured JSON on stdout; ship them with your usual collector.
  Secrets are never logged; API keys appear only as fingerprints.

**Upgrades**

- API servers and workers are stateless: rolling updates are safe. Storage
  format compatibility across versions is documented in release notes.

## 7. Bare metal / VM deployment

Run the two binaries under systemd with the same environment variables:

```ini
# /etc/systemd/system/shoal-server.service
[Unit]
Description=Shoal API server
After=network-online.target

[Service]
ExecStart=/usr/local/bin/shoal-server
EnvironmentFile=/etc/shoal/shoal.env
User=shoal
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/cache/shoal

[Install]
WantedBy=multi-user.target
```

A single node with the `fs` backend (`SHOAL_STORAGE_BACKEND=fs`,
`SHOAL_FS_ROOT=/var/lib/shoal`) is fine for testing, but use an
S3-compatible store for any deployment where durability matters.
