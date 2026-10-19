# Aether Operations Runbooks

This document covers the environments that are clearly represented in the repository today: local node execution, the process-based devnet, the Compose-based test network, and the larger Compose development stack under `deploy/docker/`.

The supported local E2E journey uses four processes via `scripts/e2e-devnet.sh`.
Compose and remote deployment assets require separate qualification; CI does
not bring up Compose. Slot progress alone is not finality: check
`aeth_getFinalizedSlot` and peer agreement as well.

## Environment Inventory

### Single Node

- Start with `cargo run -p aether-node --bin aether-node`
- Default RPC endpoint: `http://127.0.0.1:8545`
- Default health endpoint: `http://127.0.0.1:8545/health`
- Default data path: `./data/node1`

### Local Multi-Node Devnet

- Start with `./scripts/devnet.sh start`
- Logs: `./data/devnet/node*.log`
- Data: `./data/devnet/node*/`

### Optional Compose Test Assets

- Defined in `docker-compose.test.yml`
- Helper script: `./scripts/docker-test.sh`

### Compose Development Stack

- Defined in `deploy/docker/docker-compose.yml`
- Includes validators, RPC, PostgreSQL, indexer, Prometheus, Grafana, and MinIO services

## Common Health Checks

Node health:

```bash
curl -s http://127.0.0.1:8545/health
```

Slot progress:

```bash
curl -s http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"aeth_getSlotNumber","params":[],"id":1}'
```

Compose service status:

```bash
docker compose -f docker-compose.test.yml ps
docker compose -f deploy/docker/docker-compose.yml ps
```

## Scenario 1: Node Fails to Start

Checks:

1. Confirm the workspace builds:
   ```bash
   cargo build --workspace
   ```
2. Confirm the configured data directory is writable.
3. Check whether `AETHER_CONFIG_PATH`, `AETHER_NODE_DB_PATH`, `AETHER_RPC_PORT`, or `AETHER_P2P_PORT` are set to unexpected values.
4. Review stderr and recent logs.

Likely local causes:

- a stale or invalid config override;
- a port collision on `8545` or `9000`;
- a corrupted local data directory; or
- an out-of-date build artifact after switching branches.

## Scenario 2: RPC Is Up but Slots Do Not Advance

Checks:

1. Query `/health`.
2. Query `aeth_getSlotNumber` repeatedly.
3. Review node logs for consensus, storage, or lock-poisoning errors.
4. If using the multi-node devnet, verify that the peer ports are not already in use.

Useful log locations:

- single node: terminal output
- script-based devnet: `./data/devnet/node*.log`
- Compose environments: `docker compose ... logs`

## Scenario 3: Local Devnet Is Unhealthy

Checks:

1. Stop the existing devnet:
   ```bash
   ./scripts/devnet.sh stop
   ```
2. If necessary, clean it:
   ```bash
   ./scripts/devnet.sh clean
   ```
3. Restart it and inspect `./data/devnet/node*.log`.
4. Query each RPC port from `8545` through `8548`.

If the issue persists, confirm that no old `aether-node` processes are still running and that the expected ports are available.

## Scenario 4: Docker Test Network Fails

Checks:

1. Build and start the network manually:
   ```bash
   docker compose -f docker-compose.test.yml build
   docker compose -f docker-compose.test.yml up -d validator-1 validator-2 validator-3 validator-4
   ```
2. Inspect container logs:
   ```bash
   docker compose -f docker-compose.test.yml logs --tail=200
   ```
3. Re-run the test runner:
   ```bash
   docker compose -f docker-compose.test.yml run test-runner
   ```
4. Clean up:
   ```bash
   docker compose -f docker-compose.test.yml down
   ```

## Scenario 5: Reset Local State

Single-node reset:

```bash
rm -rf ./data/node1
```

Devnet reset:

```bash
./scripts/devnet.sh clean
```

Compose test-network reset:

```bash
docker compose -f docker-compose.test.yml down -v
```

Use destructive cleanup carefully. Do not remove data you intend to preserve.

## Deployment Notes

The repository contains deployment assets beyond local development:

- `deploy/docker/`
- `deploy/helm/`
- `deploy/k8s/`
- `deploy/terraform/`

Those assets should be treated as operator-reviewed infrastructure material. The current GitHub Actions workflow validates build and test paths, but it does not perform automated rollouts or production health checks for those environments.
