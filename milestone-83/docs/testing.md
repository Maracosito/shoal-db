# Testing Reef

This document describes the test suites that ship with the storage engine
milestone, how to run them, and — most importantly — exactly which recovery
guarantees they prove.

## Test layers

| Layer | Location | Backend | When it runs |
|---|---|---|---|
| Store conformance | `crates/reef-store/tests/conformance.rs` | memory, local FS, S3 (gated) | always / gated |
| Failpoint semantics | `crates/reef-store/tests/failpoint_tests.rs` | memory | always |
| Write path | `crates/reef-engine/tests/write_path_tests.rs` | memory, local FS | always |
| Crash recovery | `crates/reef-engine/tests/recovery_tests.rs` | memory + failpoints | always |
| Compaction behavior | `crates/reef-engine/tests/compaction_tests.rs` | memory + failpoints | always |
| Admin commands | `crates/reef-engine/tests/admin_tests.rs` | memory | always |
| S3 / MinIO end-to-end | `crates/reef-engine/tests/minio_integration.rs` | MinIO / any S3 | gated by env |
| In-module unit tests | `#[cfg(test)]` blocks throughout the crates | — | always |

## Running the suites

Everything that does not need external infrastructure:

```sh
cargo test --workspace --all-targets
```

Individual suites:

```sh
cargo test -p reef-store  --test conformance
cargo test -p reef-store  --test failpoint_tests
cargo test -p reef-engine --test write_path_tests
cargo test -p reef-engine --test recovery_tests
cargo test -p reef-engine --test compaction_tests
cargo test -p reef-engine --test admin_tests
```

### MinIO / S3 suites

The S3-backed tests are **skipped silently** unless `REEF_TEST_S3_ENDPOINT`
is set, so `cargo test` stays green on machines without Docker. The
recommended way to run them:

```sh
scripts/run-minio-tests.sh
```

The script starts MinIO from `deploy/docker-compose.test.yml`, creates the
`reef-test` bucket, exports the environment, runs both gated suites, and
tears MinIO down (set `KEEP_MINIO=1` to keep it running for inspection at
<http://127.0.0.1:9001>, credentials `reefadmin` / `reefsecret`).

Manual configuration, e.g. to point the suites at a real S3 bucket:

| Variable | Default | Meaning |
|---|---|---|
| `REEF_TEST_S3_ENDPOINT` | *(unset → skip)* | S3 endpoint URL |
| `REEF_TEST_S3_BUCKET` | `reef-test` | bucket name |
| `REEF_TEST_S3_REGION` | `us-east-1` | region |
| `REEF_TEST_S3_ACCESS_KEY` | `reefadmin` | access key id |
| `REEF_TEST_S3_SECRET_KEY` | `reefsecret` | secret key |

Every S3 test uses a unique, timestamped namespace (and the store
conformance suite a unique key prefix), so suites can safely share one
bucket and re-runs never collide. Test objects are small; clean the bucket
periodically if you reuse it long-term.

## What the suites prove

### Store conformance (`conformance.rs`)

The engine's correctness arguments rest on a small set of object-store
semantics. The conformance battery pins them down identically for every
backend:

- put/get roundtrips for text, binary (embedded NULs) and 1 MiB payloads;
- full-content overwrite;
- not-found errors for missing keys;
- recursive prefix listing returning *exactly* the matching keys;
- idempotent delete (deleting a missing key is a no-op);
- `put_if_absent`: first writer wins, the loser fails, and the losing write
  never modifies the object. This primitive backs the manifest commit
  protocol.

The local-FS suite additionally proves data survives dropping and reopening
the store (process restart).

### Failpoint semantics (`failpoint_tests.rs`)

`FailpointStore` wraps any store and injects faults the recovery and
compaction suites depend on. The tests fix its contract:

| Mode | Caller sees | Storage state |
|---|---|---|
| `ErrorBefore` | error | nothing written / nothing deleted |
| `ErrorAfter` | error | operation fully applied (lost ack) |
| `Truncate(n)` | error | only the first `n` bytes landed (torn write) |

Failpoints are one-shot, scoped by key substring, and `clear_failpoints()`
disarms everything.

### Crash recovery (`recovery_tests.rs`, `compaction_tests.rs`)

Together these suites prove the engine's durability contract:

1. **Acknowledged writes survive restart.** Upserts/deletes acknowledged by
   the engine are replayed from the WAL in object storage after the process
   is dropped and reopened — with no flush ever having run.
2. **Restart mid-indexing is safe.** A crash between "segment uploaded" and
   "manifest committed" leaves the WAL authoritative; replay produces
   exactly the acknowledged rows, no loss and no duplication.
3. **Restart mid-compaction is safe.** Compaction only becomes visible at
   the atomic manifest swap. `failed_manifest_write_aborts_compaction_atomically`
   shows a failed manifest PUT leaves the old manifest, the old segment set
   and all reads untouched, and that a retry succeeds.
4. **Lost manifest acks converge.** `manifest_write_with_lost_ack_is_safe_after_restart`
   covers the nastiest case: the manifest PUT landed but the writer saw an
   error. Restart reconciles to exactly the acknowledged data either way.
5. **Manifest versions are strictly monotonic** across flush, compaction and
   reopen — a reader can never observe time going backwards.
6. **Corrupt or incomplete files are detected**, via checksums and framing,
   both at read time and by `verify`.

### Compaction behavior (`compaction_tests.rs`)

- the merge policy collapses many small L0 segments into fewer segments with
  no row loss or duplication;
- a full compaction physically drops tombstones and shadowed row versions
  (post-compaction segment `doc_count`s equal the live row count exactly);
- compaction **defers deletion**: superseded segments stay in object storage
  until `gc()` so readers holding an older manifest remain correct;
- `gc()` removes only objects unreferenced by the live manifest — proven by
  a cold reopen from object storage alone after GC — and is idempotent.

### Admin commands (`admin_tests.rs`)

- `verify` is clean on healthy and empty namespaces, and detects corrupted
  segment payloads, missing segment objects, and orphaned objects left by
  crashes;
- `repair` removes orphans, quarantines unreadable segments (restoring
  invariants without ever fabricating rows), leaves healthy namespaces
  untouched, and leaves the namespace fully writable;
- `rebuild` reconstructs indexes loss-free from segments, folds any
  unflushed WAL tail, never rolls the manifest backwards, and commits its
  result durably (verified by cold reopen).

### S3 / MinIO end-to-end (`minio_integration.rs`)

Re-proves the headline guarantees against a real S3 API rather than the
in-memory store: restart durability from WAL alone, flush + compaction +
cold reopen, delete + tombstone GC, GC safety, and rebuild from the bucket
alone. This is the suite that demonstrates *indexes are reconstructable
from object storage with no local state whatsoever*.

## Continuous integration

`.github/workflows/ci.yml` runs three jobs on every push and pull request:

1. **lint** — `cargo fmt --check` and `cargo clippy -D warnings`;
2. **test** — the full workspace suite on memory and local-FS backends;
3. **minio** — the gated S3 suites against a `bitnami/minio` service
   container (chosen because GitHub service containers cannot override the
   image command, and the Bitnami image both starts the server and
   pre-creates the bucket from `MINIO_DEFAULT_BUCKETS`).

## Writing new tests

- Prefer the `MemoryStore` for logic tests — it is exact, fast and
  deterministic.
- Use `FailpointStore` for crash-shaped tests; arm a one-shot failpoint,
  assert the error, then assert the on-storage outcome by reopening the
  engine from the *inner* store.
- Anything that asserts S3 behavior belongs in a gated test: check the env,
  `eprintln!("SKIP ...")` and `return` when unset, and use unique
  namespace/prefix names so shared buckets stay safe.
- Never assert on wall-clock timing; all suites must be deterministic.
