# Shoal Threat Model

This document describes the assets Shoal protects, the trust boundaries in a
deployment, the threats we have analyzed, the mitigations built into the
system, and the residual risks an operator must manage. It applies to the
single-tenant and small-multi-tenant deployment shapes that Shoal targets in
v1. It should be read together with the
[deployment hardening guide](hardening.md).

---

## 1. System Overview

A Shoal deployment consists of:

```
                        ┌────────────────────────────┐
   clients (SDK/HTTP)   │        shoal-server         │     object storage
 ───── HTTPS ─────────► │  auth ─ routes ─ engine     │ ──► (S3 / MinIO / fs)
                        │  rate limit  audit  metrics │      WAL, segments,
                        │                             │      manifests, refs
                        │  ┌───────────────────────┐  │
                        │  │ shoal-cache           │  │
                        │  │  memory tier (hot     │  │
                        │  │  manifests/indexes)   │  │
                        │  │  disk tier (segments) │──┼──► local SSD
                        │  └───────────────────────┘  │
                        └────────────────────────────┘
                                   │
                                   ▼
                        Prometheus /metrics, logs,
                        OpenTelemetry traces, audit log
```

Key properties relevant to security:

- **Object storage is the durable source of truth.** Compute nodes are
  stateless apart from caches; anything on local disk is a re-fetchable copy
  of object-storage data.
- **Branching is copy-on-write.** Multiple namespaces (including namespaces
  belonging to different projects, if an operator copies across projects) may
  reference the same immutable segment objects. Reference counts in the
  registry decide when objects are physically deleted.
- **All client access is via the HTTP JSON API**, authenticated with API keys
  carrying a role (`admin`, `writer`, `reader`) and a scope
  (organization → project → namespace).

## 2. Assets

| Asset | Description | Confidentiality | Integrity | Availability |
|---|---|---|---|---|
| Document data | Vectors, text, metadata attributes stored in WAL files and segments | High | High | Medium |
| API keys | Bearer credentials granting read/write/admin access | Critical | Critical | Medium |
| Object-storage credentials | S3/MinIO access keys used by the server | Critical | Critical | High |
| Manifests & registry | Namespace structure, segment references, refcounts | Medium | **Critical** (corruption ⇒ data loss or cross-namespace leakage) | High |
| Audit log | Record of namespace and write operations | Medium | High | Medium |
| Disk cache contents | Plaintext copies of segments on local SSD | High | Medium | Low (re-fetchable) |
| Metrics & traces | Operational telemetry; may reveal namespace names and traffic patterns | Low–Medium | Low | Low |

## 3. Actors and Trust Boundaries

### Actors

- **Anonymous network peer** — anything that can reach the API port.
  Untrusted.
- **Reader key holder** — may query, export, and read namespace metadata
  within its scope. Semi-trusted: trusted to read its data, not to write or
  administer.
- **Writer key holder** — reader plus upsert/patch/delete and warm-cache.
- **Admin key holder** — writer plus namespace create/delete/copy/branch,
  pinning, and key management. Trusted within its organization scope.
- **Operator** — controls the host, environment variables, config files,
  object-storage credentials. Fully trusted. Shoal does **not** defend
  against a malicious operator.
- **Object storage provider** — trusted for durability and (if configured)
  encryption at rest. Shoal does not implement client-side encryption in v1;
  the provider can read stored data.

### Trust boundaries

1. **Network → API server.** Crossed by every HTTP request. Enforced by
   API-key authentication, role checks, scope checks, rate limits, and body
   size limits.
2. **API server → object storage.** Crossed by every storage I/O. Enforced by
   the object-storage provider's IAM and network controls; Shoal sends
   credentials configured by the operator.
3. **API server → local disk cache.** Cache files inherit the trust of the
   host. Anyone with filesystem access to the cache directory can read
   cached segment data.
4. **API server → telemetry sinks** (Prometheus scrape, OTLP endpoint, log
   aggregation). Telemetry must never contain secrets (see §5.6).

## 4. Threat Analysis (STRIDE)

### 4.1 Spoofing

| Threat | Vector | Mitigation |
|---|---|---|
| Forged identity via stolen/guessed API key | Network | Keys are long random strings (≥ 32 bytes of entropy recommended); the server compares keys in constant time; failed auth attempts are rate-limited per source and logged to the audit log. **Keys are bearer tokens: TLS is mandatory in any non-loopback deployment** (terminated by a reverse proxy; see hardening guide). |
| Replay of captured requests | Network | TLS prevents capture in transit. Write endpoints support idempotency via client-supplied request IDs, so an attacker replaying a captured upsert cannot multiply effects; replays of reads with a stolen key are equivalent to key theft (revoke the key). |
| Spoofed object-storage endpoint | Server-side | Operator must configure `https://` endpoints with certificate verification (default). Plain-HTTP endpoints are only acceptable for loopback MinIO in development. |

### 4.2 Tampering

| Threat | Vector | Mitigation |
|---|---|---|
| Modification of WAL/segment objects in object storage | Storage | All WAL frames and segment files carry CRC32C checksums verified on read; corrupted or truncated frames are rejected and surfaced as errors rather than silently served. Manifests are written atomically (write-new-then-swap-pointer); a partially written manifest is never referenced. Operators should additionally enable bucket versioning + restrictive IAM (hardening guide §4). |
| Tampering with cached segments on disk | Host | Cache files are written to a directory the operator must restrict to the service user (`0700`); cache reads validate the segment checksum recorded in the manifest before serving. A tampered cache entry fails validation and is re-fetched from object storage. |
| Cross-namespace tampering via branching/refcount bugs | Application | Segments are **immutable** once written; branches never write into shared segments — all branch writes go to new WAL/segment objects under the branch's own prefix. Reference counting is maintained transactionally in the registry; deletion only removes objects whose refcount reaches zero. Integration tests cover branch isolation in both directions, multi-level branches, and branch deletion (`crates/shoal-it/tests/branching.rs`). |
| Audit log tampering | Host | Audit records are append-only JSON lines with monotonically increasing sequence numbers and timestamps. Detection of removal requires shipping the log to an external sink (recommended; hardening guide §7). Shoal does not implement hash-chained audit logs in v1 (**residual risk**, see §6). |

### 4.3 Repudiation

| Threat | Mitigation |
|---|---|
| A key holder denies performing a destructive operation | Every namespace lifecycle operation (create/delete/copy/branch/pin) and every write operation (upsert/patch/delete/delete-by-filter) emits an audit record containing: timestamp, request ID, key ID (never the key itself), role, org/project/namespace, operation, outcome, and source IP. Read/query operations are auditable at `audit.level = "verbose"` (off by default for volume reasons). |

### 4.4 Information Disclosure

| Threat | Vector | Mitigation |
|---|---|---|
| Secrets leaked in logs/traces/errors | Telemetry | API keys and object-storage credentials are wrapped in redacting types: `Debug`/`Display` print `[REDACTED]`, and the `Authorization` header is stripped by the tracing layer before request logging. Error responses never echo credentials or internal storage paths to clients. Audit records reference keys by truncated fingerprint (first 8 chars of the key ID), never the secret. |
| Cross-tenant read via scope-check bypass | API | Authorization is enforced in one place: the auth extractor resolves the key to a `(role, scope)` and every route handler asserts the required role **and** that the org/project/namespace in the path is within scope, before touching the engine. Namespace existence is not revealed to out-of-scope keys (uniform `404`/`403` policy per config). |
| Cross-namespace read via shared segments | Engine | Shared segments are only reachable through a namespace's manifest; query execution resolves segment IDs strictly from the requesting namespace's manifest, never from raw object listing. Document-ID and deletion visibility are per-manifest, so a branch cannot observe source tombstones applied after the branch point and vice versa. |
| Disk cache read by other host processes | Host | Cache directory permissions + dedicated service user; full mitigation requires filesystem/volume encryption (hardening guide §5). Shoal does not encrypt cache files itself in v1 (**residual risk**). |
| Data exposure at the storage provider | Storage | Enable provider-managed encryption at rest (SSE-S3 / SSE-KMS / MinIO KES). Shoal passes through SSE headers when configured. Client-side encryption is a non-goal for v1. |
| Metrics endpoint leaks namespace names / traffic patterns | Telemetry | Per-namespace metric labels are **optional** (`metrics.per_namespace_labels = false` by default in multi-tenant configs). `/metrics` should be bound to an internal interface or protected by network policy (hardening guide §6). |
| Timing side channels in key lookup | API | Constant-time comparison of key material; key lookup is by keyed hash of the presented token, so lookup time does not depend on prefix matches. |

### 4.5 Denial of Service

| Threat | Mitigation |
|---|---|
| Request flooding | Optional token-bucket rate limits per key and per org (`rate_limit.*` config), returning `429` with `Retry-After`. Volumetric/network floods must be handled upstream (CDN/LB/WAF). |
| Oversized payloads | Hard request body limit (default 32 MiB, configurable), per-batch document count limit, per-document size limit, vector dimension limit. Limits return structured `413`/`400` errors. |
| Pathological queries (huge `top_k`, deep filters, many sub-queries) | `top_k` capped (default 1 000), filter tree depth capped (default 32), multi-query fan-out capped (default 16), per-request execution deadline (default 30 s, configurable). |
| Cache exhaustion by hostile reader | Disk and memory caches have hard byte budgets with LRU eviction; pinned namespaces have a separate budget so pins cannot be evicted by scans, and scans cannot starve pins beyond the unpinned budget. Warm-cache requests require `writer` role and are rate-limited. |
| Object-storage cost amplification (forcing cold reads) | Cold-read and object-storage I/O metrics enable alerting; quotas (`quota.max_storage_bytes`, `quota.max_namespaces`, `quota.max_docs`) bound per-org footprint. Cost-based throttling is not implemented in v1 (**residual risk** for open multi-tenant offerings). |
| Compaction/indexing starvation | Background work runs on a separate task pool with bounded concurrency; indexing lag is exported as a metric (`shoal_indexing_lag_seconds`) for alerting. |

### 4.6 Elevation of Privilege

| Threat | Mitigation |
|---|---|
| Reader → writer / writer → admin via missing role check | Role requirements are declared per route and enforced by the shared auth extractor; integration tests in `crates/shoal-it/tests/api_surface.rs` assert that every mutating endpoint rejects `reader` keys and that admin-only endpoints reject `writer` keys. |
| Scope widening via path manipulation (`../`, encoded separators) in org/project/namespace names | Names are validated against a strict charset (`[a-z0-9][a-z0-9-_]{0,62}`) at creation and on every request; storage layout keys are constructed from validated components, never from raw client strings. |
| Privilege escalation via key-management endpoints | Key creation/revocation requires `admin` within the target scope; an admin scoped to a project cannot mint org-scoped keys. |
| Container escape / host compromise | Out of Shoal's scope; mitigated by container hardening (non-root, read-only rootfs, seccomp) per the hardening guide §8. |

## 5. Built-in Security Mechanisms (summary)

1. **Authentication:** API keys, presented as `Authorization: Bearer <key>`.
   Stored server-side as keyed hashes (never plaintext at rest in the
   registry); compared in constant time.
2. **Authorization:** three roles (`admin` ⊃ `writer` ⊃ `reader`) crossed
   with hierarchical scopes (org / project / namespace). Single enforcement
   point in the auth extractor + per-route role declaration.
3. **Isolation:** per-namespace storage prefixes; manifest-mediated segment
   access; copy-on-write branching with transactional refcounts; immutable
   segments.
4. **Input validation:** name charset validation, body/batch/vector/filter
   limits, JSON schema validation with structured errors.
5. **Abuse controls:** optional per-key/per-org rate limits and quotas;
   request deadlines; bounded background work.
6. **Secret-safe telemetry:** redacting secret types, header stripping,
   key-fingerprint-only audit records, no client echo of credentials.
7. **Integrity:** CRC32C on WAL frames and segments; atomic manifest swaps;
   checksum-validated cache reads; crash-recovery tests.
8. **Auditability:** append-only structured audit log of all administrative
   and write operations, with request-ID correlation to access logs and
   traces.

## 6. Residual Risks and Explicit Non-Goals (v1)

Operators must understand and accept these, or mitigate them externally:

1. **No built-in TLS.** Shoal serves plain HTTP and expects TLS termination
   at a reverse proxy or service mesh. Running without TLS off-loopback
   exposes bearer keys.
2. **No client-side encryption.** The object-storage provider (and anyone
   with bucket credentials) can read stored data. Use provider encryption at
   rest + IAM.
3. **Unencrypted disk cache.** Use encrypted volumes if cached data is
   sensitive.
4. **Audit log is tamper-evident only by sequence gaps**, not hash-chained.
   Ship it to an external append-only sink for stronger guarantees.
5. **Single shared object-storage credential.** Shoal does not do per-tenant
   bucket credentials; a server compromise exposes all tenants' data in the
   configured bucket. For hard tenant isolation, run separate Shoal
   deployments with separate buckets.
6. **DoS protections are application-level only.** Volumetric attacks and
   sophisticated cost-amplification require upstream controls.
7. **No SSO/OIDC, no per-document ACLs, no row-level security** in v1.
   Model these in your application, or scope namespaces per end-tenant.

## 7. Security Testing

- `crates/shoal-it/tests/api_surface.rs` — role-matrix tests for every
  endpoint; out-of-scope access tests; name-validation tests.
- `crates/shoal-it/tests/branching.rs` — branch isolation in both
  directions, multi-level branches, deletion safety (refcount), query
  correctness on branches.
- `crates/shoal-it/tests/cache.rs` — cache budget enforcement, eviction,
  pinning, checksum validation of cache hits.
- `crates/shoal-it/tests/minio.rs` — full API surface against MinIO,
  including recovery after restart and corrupted-object handling.
- Unit tests in `shoal-server` cover constant-time key comparison wiring,
  redaction of secret types in `Debug`/`Display`, rate-limiter behavior, and
  audit-record schema.

Suggested external testing for production deployments: dependency audit
(`cargo audit`), fuzzing of the filter parser and WAL frame decoder
(`cargo fuzz` targets are welcome contributions), and standard web-tier
penetration testing against a staging deployment.
