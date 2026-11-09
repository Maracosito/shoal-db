# Contributing to Gannet

Thanks for your interest in contributing! Gannet is an object-storage-native
vector + full-text search database, and we welcome contributions of all
sizes — bug reports, docs fixes, tests, benchmarks, and features.

## Ground rules: clean-room policy

Gannet is an **independent clean-room implementation**. Contributors must not:

- copy source code, internal documentation, or proprietary materials from
  Turbopuffer or any other closed-source search database;
- contribute code they do not have the right to license under Apache-2.0;
- copy branding, marketing copy, or documentation text from other products.

Public, factual knowledge (papers, blog posts describing general techniques,
open standards like BM25/IVF) is fine and encouraged. If you are unsure
whether something crosses the line, open an issue and ask before submitting.

## Getting started

Prerequisites:

- Rust (the version pinned in `rust-toolchain.toml` is installed automatically
  by `rustup`)
- Docker + Docker Compose (for the local dev stack and MinIO-backed tests)

Build and test everything:

```sh
cargo build --workspace
cargo test --workspace
```

Run the local dev stack (MinIO + API server):

```sh
docker compose up -d --build
# or, once you've built the CLI:
cargo run -p gannet-cli -- dev up
cargo run -p gannet-cli -- health
```

### S3 integration tests

Unit tests run against the in-memory and filesystem storage backends and need
no external services. The S3 storage conformance tests run against MinIO and
are gated on environment configuration — see
`crates/gannet-core/tests/storage_conformance.rs` for the exact variables. A
typical local invocation after `docker compose up -d`:

```sh
AWS_ACCESS_KEY_ID=gannet AWS_SECRET_ACCESS_KEY=gannet-dev-secret \
  cargo test --workspace -- --include-ignored
```

## Code standards

- `cargo fmt --all` before committing; CI rejects unformatted code.
- `cargo clippy --workspace --all-targets` must be warning-free.
- New behavior requires tests. Bug fixes require a regression test that fails
  without the fix.
- Public APIs (Rust items, HTTP endpoints) need doc comments / OpenAPI updates.
- Never log secrets. Configuration types must not carry credentials (see
  `crates/gannet-server/src/config.rs` for the enforced pattern).
- Storage-format changes must update `docs/storage-format.md` and follow its
  versioning rules; format changes without a spec update will not be merged.

## Commit and PR process

1. Fork and create a topic branch (`feat/...`, `fix/...`, `docs/...`).
2. Keep commits focused; write imperative-mood messages
   (`Add IVF centroid persistence`, not `Added stuff`).
3. Sign off each commit (`git commit -s`) to certify the
   [Developer Certificate of Origin](https://developercertificate.org/).
   By signing off you certify that you wrote the change or otherwise have the
   right to submit it under Apache-2.0.
4. Open a PR describing **what** changed and **why**. Link related issues.
5. A maintainer will review; please respond to feedback within the PR rather
   than opening replacement PRs.

## Project layout

| Path                   | Contents                                            |
|------------------------|-----------------------------------------------------|
| `crates/gannet-core`   | Engine: storage abstraction, formats, indexes       |
| `crates/gannet-server` | HTTP API server (axum)                              |
| `crates/gannet-cli`    | `gannet` developer CLI                              |
| `api/openapi.yaml`     | OpenAPI 3.1 specification (source of truth for API) |
| `docs/`                | Architecture, storage format, operations docs       |
| `config/`              | Example and dev-stack configuration files           |

## Reporting issues

- **Bugs**: include version, storage backend, reproduction steps, and relevant
  logs (scrub credentials and bucket names if sensitive).
- **Security issues**: do **not** open a public issue. Email the maintainers
  (see repository metadata) and allow time for a fix before disclosure.

## License

By contributing, you agree that your contributions are licensed under the
Apache License, Version 2.0, the same license that covers the project.
