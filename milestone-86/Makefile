# Shoal developer entrypoints.
#
# These targets are thin wrappers around the same commands CI runs
# (.github/workflows/ci.yml and e2e.yml), so "works in make" should mean
# "works in CI". See docs/cli.md and e2e/README.md for details on
# environment variables consumed by the dev stack and e2e suites.

SHELL := /bin/bash
COMPOSE := docker compose -f deploy/docker/docker-compose.yml

.PHONY: help build build-release lint fmt test test-rust test-python test-ts \
        dev-up dev-down dev-logs e2e-python e2e-ts helm-lint clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

## ----- Build -----

build: ## Build all Rust crates (debug)
	cargo build --workspace

build-release: ## Build all Rust crates (release)
	cargo build --workspace --release

## ----- Lint / format -----

lint: ## Run all linters (Rust, Python, TypeScript)
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd sdks/python && python -m ruff check .
	cd sdks/typescript && npm run lint

fmt: ## Auto-format Rust and Python sources
	cargo fmt --all
	cd sdks/python && python -m ruff format .

## ----- Unit tests -----

test: test-rust test-python test-ts ## Run all unit test suites

test-rust: ## Rust workspace tests
	cargo test --workspace

test-python: ## Python SDK tests (install the SDK first: pip install -e "sdks/python[dev]")
	cd sdks/python && python -m pytest tests

test-ts: ## TypeScript SDK tests
	cd sdks/typescript && npm install && npm test

## ----- Local dev stack -----

dev-up: ## Start the full local stack (API, worker, MinIO, example app)
	$(COMPOSE) up -d --build

dev-down: ## Stop the local stack and remove volumes
	$(COMPOSE) down -v

dev-logs: ## Tail logs from the local stack
	$(COMPOSE) logs -f

## ----- End-to-end tests (require a running stack; see e2e/README.md) -----

e2e-python: ## Python SDK e2e suite against a running server
	cd e2e/python && python -m pip install -r requirements.txt && python -m pytest

e2e-ts: ## TypeScript SDK e2e suite against a running server
	cd e2e/typescript && npm install && npm test

## ----- Deployment packaging -----

helm-lint: ## Lint the Helm chart
	helm lint deploy/helm/shoal

clean: ## Remove build artifacts
	cargo clean
	rm -rf sdks/python/dist sdks/python/build sdks/python/*.egg-info
	rm -rf sdks/typescript/dist sdks/typescript/node_modules
	rm -rf e2e/typescript/node_modules
