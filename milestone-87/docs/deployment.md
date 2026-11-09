# Lagoon Deployment Guide

This guide covers running Lagoon from a laptop to production: configuration,
object-storage setup, encryption at rest, Kubernetes, monitoring, and
hardening.

## 1. Deployment Topologies

| Topology | What runs | Use for |
|---|---|---|
| **Single process** | `lagoon-server --embedded-indexer`, filesystem or MinIO storage | development, tests, small workloads |
| **Compose stack** | server + worker + MinIO (see `deploy/docker-compose.yml`) | local development matching production shape |
| **Production** | N stateless servers behind a load balancer + M workers + managed object storage (S3/R2/GCS) | real workloads |

Compute nodes are stateless: they hold only caches. Any node can be replaced
at any time; durable state lives entirely in the bucket.

## 2. Configuration

Configuration comes from `lagoon.toml` and/or environment variables
(`LAGOON_` prefix; env wins). Key settings:

```toml
[server]
listen = "0.0.0.0:8080"
embedded_indexer = false          # true for single-process mode

[storage]
backend = "s3"                    # "s3" | "fs"
bucket = "lagoon-prod"
prefix = "lagoon/"
region = "us-east-1"
endpoint = ""                     # set for MinIO/R2, e.g. "http://minio:9000"
# Credentials: standard AWS env/instance-profile chain
# (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / IAM role). Never put secrets here.
sse = "aws:kms"                   # "" | "AES256" | "aws:kms"  — see §4
sse_kms_key_id = "arn:aws:kms:us-east-1:123456789012:key/..."

[cache]
dir = "/var/lib/lagoon/cache"
disk_bytes = 53687091200          # 50 GiB
memory_bytes = 4294967296         # 4 GiB
max_pinned_bytes = 21474836480    # 20 GiB

[indexer]
flush_interval_secs = 5
compaction_fanin = 8

[manifest]
poll_interval_ms = 1000           # read staleness bound on non-writer nodes

[auth]
keys_file = "/etc/lagoon/keys.toml"   # key-id -> {hash, org, role}; managed via `lagoon keys`

[limits]                          # optional; absent = unlimited
requests_per_second = 200
write_bytes_per_minute = 1073741824
max_namespaces = 1000

[observability]
log_format = "json"
otlp_endpoint = ""                # set to enable OpenTelemetry trace export
audit_log = "/var/log/lagoon/audit.jsonl"
```

Environment equivalents: `LAGOON_STORAGE__BUCKET`, `LAGOON_STORAGE__SSE`,
`LAGOON_CACHE__DIR`, etc. (double underscore = nesting).

## 3. Object Storage Setup

### AWS S3
- Dedicated bucket, **versioning recommended** (protects `manifest/CURRENT`
  against operator error; Lagoon itself does not require it).
- Block public access. Grant the compute role only:
  `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:ListBucket` on the
  Lagoon prefix.
- Lifecycle rule: abort incomplete multipart uploads after 1 day.

### MinIO (local / self-hosted)
```bash
docker compose -f deploy/docker-compose.yml up -d
# brings up MinIO (console at :9001), lagoon-server, lagoon-worker
```

### Filesystem mode
`backend = "fs"` with `[storage].root = "/data/lagoon"` — for tests and
single-machine development. Atomic-rename is used in place of conditional
PUTs. Not recommended for multi-node deployments (no shared CAS).

## 4. Encryption at Rest (provider-managed SSE)

Lagoon delegates encryption at rest to the object-storage provider — this is
deliberate: provider SSE is mature, audited, and free of client-side key
distribution problems.

**AWS S3:**

- `sse = "AES256"` → SSE-S3 (S3-managed keys). Simplest; on by default for new
  S3 buckets since 2023, but Lagoon still sets the header explicitly so
  intent is auditable.
- `sse = "aws:kms"` + `sse_kms_key_id` → SSE-KMS with your CMK. Gives you key
  rotation, CloudTrail key-usage auditing, and the ability to revoke access by
  disabling the key. The compute role needs `kms:GenerateDataKey` and
  `kms:Decrypt` on the key.
- Enforce at the bucket level so misconfigured clients cannot write
  plaintext — bucket policy denying `s3:PutObject` unless
  `s3:x-amz-server-side-encryption` is present:

```json
{
  "Effect": "Deny",
  "Principal": "*",
  "Action": "s3:PutObject",
  "Resource": "arn:aws:s3:::lagoon-prod/*",
  "Condition": { "Null": { "s3:x-amz-server-side-encryption": "true" } }
}
```

**MinIO:** configure KES (MinIO's key-encryption service) and enable
bucket-default SSE; Lagoon's `sse = "AES256"` header works unchanged.

**GCS / R2:** encrypt at rest by default with provider-managed keys; GCS
supports CMEK via bucket configuration — no Lagoon configuration needed beyond
leaving `sse = ""`.

**What SSE does *not* cover:** the local disk cache. If your threat model
includes node-disk theft, place `cache.dir` on an encrypted volume (LUKS,
EBS encryption, etc.). Cache contents are reconstructible and may be wiped at
any time. In-transit encryption: terminate TLS at your load balancer or run
the server behind a TLS-terminating proxy; use `endpoint = "https://..."` to
the object store.

## 5. Kubernetes

Manifests in `deploy/k8s/` (and a Helm chart in `deploy/helm/lagoon/`):

- `Deployment` for `lagoon-server` (HPA-friendly; readiness =
  `/readyz`, liveness = `/healthz`) with an `emptyDir` or local-SSD volume for
  the cache.
- `Deployment` for `lagoon-worker` (1–2 replicas is usually plenty; workers
  coordinate via manifest CAS so duplicates are safe, just wasteful).
- `Secret` for API keys file; credentials via IRSA/Workload Identity, not
  static keys, where possible.
- `ServiceMonitor` for Prometheus scraping `/metrics`.

Sizing starting point: servers — 4 vCPU / 8 GiB RAM / 100 GiB local SSD cache
per ~50 GiB of hot namespaces; workers — 2 vCPU / 4 GiB. Measure with the
benchmark suite before committing.

## 6. Observability

- **Logs:** structured JSON to stdout (`log_format = "json"`); request logs
  include `request_id`, key ID (never the key), namespace, latency, status.
- **Audit log:** append-only JSONL of namespace lifecycle and write operations
  (`who, what, when, namespace, doc counts`) at `observability.audit_log`.
- **Metrics** (Prometheus, `/metrics`), the ones to alert on:

| Metric | Meaning |
|---|---|
| `lagoon_query_duration_seconds{quantile,kind}` | query latency histogram (vector/fts/hybrid) |
| `lagoon_cache_hits_total` / `lagoon_cache_misses_total{layer}` | memory/disk cache effectiveness |
| `lagoon_indexing_lag_seconds` | age of oldest unflushed WAL chunk |
| `lagoon_wal_tail_bytes` / `lagoon_segment_count{namespace}` | write-path backlog |
| `lagoon_compaction_running` / `lagoon_compaction_total{outcome}` | compaction health |
| `lagoon_objstore_requests_total{op}` / `lagoon_objstore_errors_total` | object-storage traffic & failures |

- **Tracing:** set `observability.otlp_endpoint` to export OpenTelemetry spans
  for the query path (plan, per-segment execution, object-store fetches).

Alert suggestions: `indexing_lag_seconds > 60`, objstore error rate > 1%,
disk-cache hit rate < 80% on pinned namespaces, p99 query latency budget.

## 7. Hardening Checklist & Threat Model

**Threat model (summary).** Trusted: the object-storage provider, the compute
hosts, holders of admin keys. Untrusted: the network (use TLS), API clients
(authenticated + role-limited + optional rate limits), other bucket tenants
(use a dedicated bucket/prefix + IAM). Out of scope for v1: malicious
co-tenants on shared compute, client-side encryption, per-document ACLs.

Checklist:

- [ ] TLS in front of the API server; HSTS at the proxy.
- [ ] Distinct API keys per application; `reader` keys for query-only paths.
- [ ] Bucket policy enforcing SSE (§4) and denying public access.
- [ ] IAM scoped to the Lagoon prefix only; no wildcard buckets.
- [ ] Rate limits enabled for internet-facing deployments.
- [ ] Cache directory on an encrypted volume if node-disk exposure matters.
- [ ] Audit log shipped to your log pipeline; alert on namespace deletions.
- [ ] Object-storage versioning + periodic `lagoon export` backups for
      business-critical namespaces.
- [ ] Run `lagoon repair --dry-run` after any storage incident.

## 8. Upgrades & Backups

- File formats are versioned; servers read all formats ≤ their own version.
  Upgrade servers before workers; rolling upgrades are safe because all
  cross-node coordination is through versioned manifests.
- Backup = the bucket. For point-in-time protection use bucket versioning or
  periodic `lagoon export --namespace <ns> --out backup.jsonl`. Branching
  (`POST .../branch`) is also a cheap pre-migration snapshot.
