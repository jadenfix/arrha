# ============================================================================
# AETHER - Build & Operations Makefile
# ============================================================================
# PURPOSE: Unified build, test, deploy interface for Aether blockchain
#
# WORKFLOW:
# Development: make build → make test → make devnet
# Validation: make test (local E2E); deployment targets require operator review
# ============================================================================

.PHONY: all build test build-web clean devnet testnet docs chaos validator-deploy \
        bench bench-parallel bench-consensus bench-mempool bench-ledger bench-storage bench-merkle \
        bench-runtime bench-crypto bench-da bench-rpc bench-types bench-staking bench-amm bench-governance deny audit

# Default target
all: build test

# Build all crates
build:
	cargo build --release

# Development build (faster, debug symbols)
dev:
	cargo build

# End-to-end test journeys
test:
	./cli-test

# Build web workspaces (no unit-test runners)
build-web:
	npm ci
	npm run build:web

# Supply chain checks (cargo-deny)
deny:
	cargo deny check bans sources

# Security advisory audit
audit:
	cargo audit

# Lint and format
lint:
	./cli-format

# Format code
fmt:
	cargo fmt --all

# Generate documentation
docs:
	cargo doc --no-deps --all

# Clean build artifacts
clean:
	cargo clean
	docker compose -f deploy/docker/docker-compose.yml down -v

# ============================================================================
# Local Development
# ============================================================================

# Generate user signing keys (validator keys come from genesis-ceremony)
keys:
	mkdir -p keys/
	cargo run -p aether-cli --bin aetherctl -- keys generate --out keys/account1.json
	cargo run -p aether-cli --bin aetherctl -- keys generate --out keys/account2.json
	cargo run -p aether-cli --bin aetherctl -- keys generate --out keys/account3.json
	cargo run -p aether-cli --bin aetherctl -- keys generate --out keys/account4.json

# Start/stop the supported four-process local devnet
devnet:
	./scripts/devnet.sh start

devnet-stop:
	./scripts/devnet.sh stop

devnet-logs:
	tail -f data/devnet/node*.log

# Generate a shared multi-validator genesis
genesis:
	cargo run -p aether-node --bin genesis-ceremony -- --help

# Run the local faucet request service
faucet:
	cargo run -p aether-faucet --bin server

# ============================================================================
# Testing & Benchmarking
# ============================================================================

# Run load generator
loadtest:
	cargo run -p aether-loadgen --release -- run \
		--rpc http://localhost:8545 \
		--mode transfer --rate 100 \
		--duration 300

# Run chaos tests
chaos:
	./scripts/chaos/run-chaos-suite.sh

# Benchmark parallel execution (legacy alias)
bench-parallel:
	cargo bench --package aether-runtime --bench scheduler

# Individual benchmark targets
bench-consensus:
	cargo bench --package aether-consensus

bench-mempool:
	cargo bench --package aether-mempool

bench-ledger:
	cargo bench --package aether-ledger

bench-storage:
	cargo bench --package aether-state-storage

bench-merkle:
	cargo bench --package aether-state-merkle

bench-runtime:
	cargo bench --package aether-runtime

bench-crypto:
	cargo bench --package aether-crypto-primitives
	cargo bench --package aether-crypto-vrf

bench-da:
	cargo bench --package aether-da-erasure
	cargo bench --package aether-da-turbine

bench-rpc:
	cargo bench --package aether-rpc-json

bench-types:
	cargo bench --package aether-types

bench-staking:
	cargo bench --package aether-program-staking

bench-amm:
	cargo bench --package aether-program-amm

bench-governance:
	cargo bench --package aether-program-governance

# Run all benchmark suites across the workspace
bench:
	@echo "==> Running all criterion benchmarks"
	cargo bench --package aether-consensus
	cargo bench --package aether-mempool
	cargo bench --package aether-ledger
	cargo bench --package aether-state-storage
	cargo bench --package aether-state-merkle
	cargo bench --package aether-runtime
	cargo bench --package aether-crypto-primitives
	cargo bench --package aether-crypto-vrf
	cargo bench --package aether-da-erasure
	cargo bench --package aether-da-turbine
	cargo bench --package aether-rpc-json
	cargo bench --package aether-types
	cargo bench --package aether-program-staking
	cargo bench --package aether-program-amm
	cargo bench --package aether-program-governance
	@echo "==> All benchmarks complete"

# ============================================================================
# Deployment (K8s)
# ============================================================================

# Deploy validator manifests
validator-deploy:
	kubectl apply -f deploy/k8s/validator/

# Deploy testnet validator
testnet-deploy: validator-deploy

# Deploy mainnet validator
mainnet-deploy: validator-deploy

# Deploy monitoring stack
monitoring:
	helm upgrade --install prometheus deploy/helm/monitoring/ \
		--namespace aether-monitoring --create-namespace

# ============================================================================
# AI Mesh
# ============================================================================

# Generate model hash
model-hash:
	@test -n "$(MODEL_PATH)" || (echo "MODEL_PATH is required" >&2; exit 1)
	shasum -a 256 "$(MODEL_PATH)" | awk '{print $$1}'

# ============================================================================
# Utilities
# ============================================================================

# Check chain status
status:
	cargo run -p aether-cli --bin aetherctl -- chain info

# Show supported chain commands
chain-help:
	cargo run -p aether-cli --bin aetherctl -- chain --help
