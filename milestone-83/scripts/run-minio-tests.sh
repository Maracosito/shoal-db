#!/usr/bin/env bash
# Runs every S3-gated test suite against a local MinIO started via Docker.
#
#   scripts/run-minio-tests.sh            # start MinIO, run tests, tear down
#   KEEP_MINIO=1 scripts/run-minio-tests.sh   # leave MinIO running afterwards
#
# Requires: docker (with the compose plugin), cargo.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

COMPOSE=(docker compose -f deploy/docker-compose.test.yml)

cleanup() {
  if [[ "${KEEP_MINIO:-0}" != "1" ]]; then
    echo "==> tearing down MinIO (set KEEP_MINIO=1 to keep it running)"
    "${COMPOSE[@]}" down -v >/dev/null 2>&1 || true
  else
    echo "==> leaving MinIO running (KEEP_MINIO=1)"
  fi
}
trap cleanup EXIT

echo "==> starting MinIO"
"${COMPOSE[@]}" up -d --wait minio

echo "==> creating test bucket"
"${COMPOSE[@]}" run --rm minio-init

export REEF_TEST_S3_ENDPOINT="${REEF_TEST_S3_ENDPOINT:-http://127.0.0.1:9000}"
export REEF_TEST_S3_BUCKET="${REEF_TEST_S3_BUCKET:-reef-test}"
export REEF_TEST_S3_REGION="${REEF_TEST_S3_REGION:-us-east-1}"
export REEF_TEST_S3_ACCESS_KEY="${REEF_TEST_S3_ACCESS_KEY:-reefadmin}"
export REEF_TEST_S3_SECRET_KEY="${REEF_TEST_S3_SECRET_KEY:-reefsecret}"

echo "==> reef-store conformance suite against MinIO"
cargo test -p reef-store --test conformance s3 -- --nocapture

echo "==> reef-engine integration suite against MinIO"
cargo test -p reef-engine --test minio_integration -- --nocapture

echo "==> all MinIO-backed tests passed"
