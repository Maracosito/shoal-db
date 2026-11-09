# Shoal Deployment Hardening Guide

This guide describes how to deploy Shoal securely. It complements the
[threat model](threat-model.md). The short version:

> Put TLS in front of it, lock down the bucket, protect the API keys and the
> cache directory, bound resource usage with rate limits and quotas, and ship
> the audit log somewhere append-only.

Everything below assumes the configuration file format documented in
`crates/shoal-server/src/config.rs` (TOML, overridable via `SHOAL_*`
environment variables).

---

## 1. Deployment Topologies

### 1.1 Local development (low security, loopback only)

- `server.bind = "127.0.0.1:8080"`, MinIO on loopback, plain HTTP is fine.
- Use throwaway keys; never reuse development keys in production.

### 1.2 Single node, production

```
internet ──► reverse proxy (TLS, HTTP/2, body limits) ──► shoal-server (loopback or private IP)
                                                              │
                                                              ▼
                                                       S3 / MinIO (private network, TLS)
```

- Terminate TLS at nginx/Caddy/Traefik/ALB. Shoal must **not** be directly
  reachable from untrusted networks.
- Bind Shoal to `127.0.0.1` or a private interface:
  `server.bind = "127.0.0.1:8080"`.

### 1.3 Kubernetes

- Expose via an Ingress/Gateway with TLS; do not use a `LoadBalancer`
  Service pointing straight at the pod.
- Apply a `NetworkPolicy` allowing ingress only from the ingress controller
  and Prometheus, and egress only to object storage, the OTLP collector, and
  DNS.
- Separate query-serving and indexer deployments can share the same image
  with different `SHOAL_ROLE`-style configuration; give the indexer no
  ingress at all.

## 2. TLS

Shoal does not terminate TLS itself in v1. Requirements:

1. **All non-loopback client traffic must be HTTPS.** API keys are bearer
   tokens; plaintext HTTP exposes them to any on-path observer.
2. **Object-storage traffic must be HTTPS** (`storage.endpoint =
   "https://..."`). Certificate verification is on by default; do not
   disable it outside loopback development.
3. At the proxy, set conservative limits that match or are tighter than
   Shoal's own: request body ≤ `server.max_body_bytes` (default 32 MiB),
   header size limits, and idle/read timeouts slightly above
   `server.request_deadline_secs`.
4. Strip and re-set `X-Forwarded-For` at the trusted edge so audit-log
   source IPs are trustworthy. Set `server.trusted_proxies` to the proxy's
   address range so Shoal honors forwarded headers **only** from it.

## 3. API Keys and Secrets

### 3.1 Generating and storing keys

- Generate keys with ≥ 32 bytes of entropy, e.g.
  `openssl rand -base64 32`. The CLI's `shoal keys create` does this for you
  and prints the secret **once**.
- Shoal stores only a keyed hash of each key in the registry. There is no
  way to recover a lost key — create a new one and revoke the old.
- Audit logs and metrics reference keys by their key ID / fingerprint, never
  the secret.

### 3.2 Role and scope discipline

- Issue the **narrowest** key that works: a query-only service gets a
  `reader` key scoped to one namespace, not a project-wide `writer`.
- Reserve `admin` keys for provisioning automation and break-glass use.
  Never embed admin keys in application code.
- Issue separate keys per consuming service so revocation is surgical and
  audit attribution is meaningful.

### 3.3 Rotation and revocation

- Rotate keys on a schedule (90 days is a sane default) and immediately on
  suspected exposure or personnel change.
- Rotation procedure: create the new key → deploy it to consumers → confirm
  the old key's last-used timestamp goes stale (`GET /v1/keys`, admin) →
  revoke the old key. Revocation is immediate; there is no cache of valid
  keys beyond the in-process registry, which is invalidated on revoke.

### 3.4 Supplying secrets to the server

- Provide the object-storage credentials and the bootstrap admin key via
  environment variables (`SHOAL_STORAGE__ACCESS_KEY`,
  `SHOAL_STORAGE__SECRET_KEY`, `SHOAL_AUTH__BOOTSTRAP_ADMIN_KEY`) or
  files referenced by `*_FILE` variants — **not** baked into images or
  committed config files.
- In Kubernetes, use `Secret` objects mounted as env vars or files; enable
  encryption at rest for etcd. Prefer an external secret manager
  (Vault, AWS Secrets Manager, SOPS) feeding those Secrets.
- Shoal redacts secret-typed values from logs, traces, panics, and error
  responses. Do not undermine this by logging raw config dumps from wrapper
  scripts, and never pass secrets as CLI arguments (visible in `ps`).

## 4. Object Storage

Object storage holds all durable data. Treat the bucket as the crown jewels.

### 4.1 IAM / access policy

Grant the Shoal service identity the **minimum** S3 actions on **only** its
bucket/prefix:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::YOUR-SHOAL-BUCKET/*"
    },
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::YOUR-SHOAL-BUCKET"
    }
  ]
}
```

- No `s3:*`, no bucket-policy or ACL permissions, no access to other
  buckets.
- On AWS, prefer IAM roles (instance profile / IRSA) over static access
  keys; Shoal picks up the default AWS credential chain when explicit keys
  are not configured.
- Block all public access at the bucket and account level.

### 4.2 Encryption at rest

Shoal relies on provider-managed encryption:

- **AWS S3:** enable default bucket encryption. SSE-S3 (`AES256`) is the
  minimum; SSE-KMS with a customer-managed key adds key-usage auditing and
  revocability. If you require request-level SSE-KMS headers, set
  `storage.sse = "aws:kms"` and `storage.sse_kms_key_id`; Shoal attaches the
  corresponding headers on every `PutObject`.
- **MinIO:** deploy MinIO with [KES](https://min.io/docs/kes) and a KMS
  backend, and enable auto-encryption (`MINIO_KMS_AUTO_ENCRYPTION=on`) so
  all objects are encrypted regardless of client headers.
- **Other S3-compatibles:** consult the provider; if no server-side
  encryption is available, use encrypted underlying volumes and accept the
  weaker model, or choose a different provider for sensitive data.

Note the limits: provider-side encryption protects disks and decommissioned
hardware, not a compromised credential. Client-side encryption is a v1
non-goal (threat model §6.2).

### 4.3 Durability and tamper resilience

- Enable **bucket versioning**. Shoal writes immutable objects and atomic
  manifest swaps, but versioning turns an accidental or malicious deletion
  into a recoverable event.
- Optionally enable Object Lock (compliance/governance mode) on a
  replica/backup bucket for ransomware resilience; do not enable it on the
  live bucket, since Shoal's compaction legitimately deletes
  zero-refcount objects.
- Configure lifecycle rules to expire noncurrent versions after your
  recovery window (e.g. 30 days) to bound cost.
- Set up cross-region replication or scheduled `mc mirror`/`aws s3 sync`
  backups if your durability requirements exceed a single region.

### 4.4 Network

- Reach S3 via VPC endpoints / private endpoints where available, so storage
  traffic never transits the public internet.
- Self-hosted MinIO must live on a private network with TLS; never expose
  the MinIO API or console publicly. Change MinIO's root credentials from
  defaults and create a dedicated service account for Shoal.

## 5. Local Disk Cache

The disk cache (`cache.disk.dir`) contains plaintext copies of segment data.

- Run Shoal as a dedicated non-root user; create the cache directory with
  mode `0700` owned by that user. Shoal refuses to start if the cache
  directory is world-writable (`cache.disk.permissive = false`, the
  default).
- Place the cache on its **own filesystem or volume** so eviction-resistant
  growth can never fill the root filesystem; set `cache.disk.max_bytes`
  to ≤ 90% of that volume.
- If document data is sensitive, use an encrypted volume (LUKS/dm-crypt,
  EBS encryption, encrypted PD) for the cache path. Shoal does not encrypt
  cache files itself.
- The cache is disposable: on suspected compromise or before
  decommissioning a node, delete the directory (and, for strict data
  destruction requirements, rely on volume encryption rather than file
  deletion). Shoal rebuilds the cache from object storage transparently.
- Memory-tier contents (`cache.memory.max_bytes`) are subject to swap;
  disable swap or use encrypted swap on hosts handling sensitive data.

## 6. Network Exposure of Auxiliary Endpoints

| Endpoint | Exposure rule |
|---|---|
| `/v1/*` API | Only via the TLS proxy. |
| `/metrics` | Internal only. Bind metrics to a separate listener (`metrics.bind = "127.0.0.1:9464"` or a private interface) and allow only Prometheus via firewall/NetworkPolicy. Metrics can reveal namespace names if `metrics.per_namespace_labels = true`; leave it `false` in multi-tenant deployments. |
| `/healthz`, `/readyz` | May be exposed to the load balancer; they return no sensitive data and require no auth. |
| OTLP export | Egress to your collector only; use TLS to the collector. Traces include request IDs, routes, and scope identifiers — treat your tracing backend as containing medium-sensitivity data. |

## 7. Rate Limits, Quotas, and Audit Logs

### 7.1 Rate limits

Enable rate limiting in any deployment where key holders are not fully
trusted:

```toml
[rate_limit]
enabled = true
# token bucket per API key
per_key_rps = 100
per_key_burst = 200
# aggregate per organization
per_org_rps = 500
per_org_burst = 1000
# stricter bucket for expensive ops (warm-cache, export, copy, branch)
expensive_rps = 2
expensive_burst = 4
```

Rejected requests get `429` with `Retry-After`; rejections are counted in
`shoal_rate_limited_total{scope=...}` — alert on sustained nonzero rates.

### 7.2 Quotas

Bound per-organization footprint so one tenant cannot exhaust storage or
namespace counts:

```toml
[quota]
enabled = true
max_namespaces_per_project = 100
max_docs_per_namespace = 10_000_000
max_storage_bytes_per_org = 500_000_000_000  # 500 GB logical
max_pinned_bytes_per_org = 20_000_000_000    # bound pin pressure on cache
```

Quota state is derived from manifests and enforced at write/namespace-create
time; exceeding a quota yields a structured `429 quota_exceeded` error.

### 7.3 Audit logs

- Audit records are JSON lines written to `audit.path` (or stdout with
  `audit.sink = "stdout"` for log-collector setups). Each record carries a
  sequence number; a gap indicates loss or tampering.
- **Ship audit logs off-host** (Loki, CloudWatch, an append-only bucket via
  your log shipper) so a host compromise cannot silently erase history.
- Retain audit logs per your compliance needs; they contain namespace names
  and key fingerprints but never document contents or key secrets.
- Set `audit.level = "verbose"` to additionally record queries/exports when
  investigating an incident; expect significant volume.

## 8. Process and Container Hardening

Recommended container settings (the shipped `Dockerfile` and Compose files
follow these):

```yaml
# docker-compose snippet
services:
  shoal:
    user: "10001:10001"          # non-root
    read_only: true              # read-only rootfs
    tmpfs: [/tmp]
    volumes:
      - shoal-cache:/var/lib/shoal/cache   # the only writable path
    cap_drop: [ALL]
    security_opt:
      - no-new-privileges:true
```

Kubernetes equivalent:

```yaml
securityContext:
  runAsNonRoot: true
  runAsUser: 10001
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities: { drop: ["ALL"] }
  seccompProfile: { type: RuntimeDefault }
```

Additional guidance:

- Set memory limits comfortably above
  `cache.memory.max_bytes + working headroom`; Shoal's memory cache budget
  is a data budget, not a total-process budget.
- Pin images by digest in production manifests; rebuild regularly to pick up
  base-image CVE fixes.
- Run `cargo audit` (RustSec) in CI; the repository CI workflow includes it.

## 9. Operational Security Checklist

Before going to production, verify:

- [ ] Shoal is reachable only through a TLS-terminating proxy; direct port
      access is blocked.
- [ ] `storage.endpoint` is `https://` with cert verification on.
- [ ] Bucket: private, versioned, default encryption enabled, minimal IAM
      policy, no public access.
- [ ] Bootstrap admin key rotated after provisioning; all consumer keys are
      least-privilege `reader`/`writer` keys with per-service issuance.
- [ ] Secrets delivered via env/secret manager; no secrets in images, repos,
      CLI args, or config files in version control.
- [ ] Cache directory: dedicated volume, `0700`, non-root service user,
      `max_bytes` set; encrypted volume if data is sensitive.
- [ ] `/metrics` not publicly reachable; `metrics.per_namespace_labels`
      appropriate for your tenancy model.
- [ ] Rate limits and quotas enabled and load-tested against expected
      traffic.
- [ ] Audit logs shipped off-host with alerting on sequence gaps and on
      `auth_failed` spikes.
- [ ] Alerts configured on: auth failure rate, `429` rate, indexing lag,
      cold-hit ratio collapse, object-storage error rate, disk-cache volume
      usage.
- [ ] Restore drill performed: rebuild a node from a clean machine using
      only the bucket (no cache, no local state) and verify queries succeed.
- [ ] Incident runbook covers: key revocation, bucket credential rotation,
      cache purge, and point-in-time recovery via bucket versioning.

## 10. Incident Response Quick Reference

| Scenario | Immediate actions |
|---|---|
| API key leaked | Revoke the key (`shoal keys revoke <id>` or `DELETE /v1/keys/{id}`); review audit log for the key's fingerprint; issue replacement. |
| Object-storage credential leaked | Rotate at the provider; update Shoal's secret and restart; review bucket access logs (enable S3 server access logging / CloudTrail data events ahead of time). |
| Host compromise | Drain and terminate the node; purge or destroy its cache volume; rotate all secrets the node held; nodes are stateless, so replacement is cheap. |
| Suspected data tampering in bucket | Use bucket versioning to diff/restore objects; Shoal's checksums will reject corrupted segments at read time — correlate `shoal_checksum_failures_total` with the timeline. |
| Accidental namespace deletion | If within the versioning retention window, restore the namespace prefix's deleted objects and re-register via `shoal admin repair` (manifest re-adoption). |
