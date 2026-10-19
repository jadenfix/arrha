# Arrha / Aether operations runbooks

The supported validation path is a local four-validator process network. See
[testing](../testing.md) for its coverage and [architecture](../architecture.md)
for the components assembled by the node binary. Package names, binaries, RPC
methods, and environment variables retain the Aether name.

## Start and inspect a local network

Run these commands from the repository root. Rust/Cargo, a C/C++ build toolchain,
`curl`, and Python 3 are required for the process journey.

```bash
./scripts/devnet.sh start
./scripts/devnet.sh status
```

`start` builds release node binaries, stops the processes recorded in its PID
files, deletes the selected devnet directory, generates genesis/validator keys,
and starts four validators. It waits up to 90 seconds for nonzero finalized
slots. **Starting again resets this network.** To rejoin one existing validator,
use `restart` only after stopping that validator; it retains that node's data.

Defaults in [the launcher](../../scripts/devnet.sh):

| Item | Default |
| --- | --- |
| State, genesis, keys, PID files | `data/devnet/` |
| Node logs | `data/devnet/node1.log` through `node4.log` |
| Consensus event logs | `data/devnet/node1.events.jsonl` through `node4.events.jsonl` |
| RPC ports | 8545–8548 |
| P2P TCP ports | 9000–9003 |
| Metrics ports | 9100–9103 |

`DEVNET_DIR`, `AETHER_BIN_DIR`, `VALIDATORS` (1–4), `BASE_RPC_PORT`,
`BASE_P2P_PORT`, and `BASE_METRICS_PORT` configure the launcher. `SKIP_BUILD=1`
reuses binaries. Keep networks in separate directories and on separate ports.
The E2E script itself assumes RPC ports 8545–8548, so leave base ports at their
defaults when running it.

```bash
curl -fsS http://127.0.0.1:8545/health
curl -fsS http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"aeth_getNodeInfo","params":[],"id":1}'
curl -fsS http://127.0.0.1:8545 \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"aeth_getFinalizedSlot","params":[],"id":1}'
```

Check each validator. A successful health request or advancing proposed slot is
insufficient evidence of finality. Compare finalized progress and the block hash
at a common finalized slot, as [the E2E script](../../scripts/e2e-devnet.sh) does.

## Validate transfers, contracts, and recovery

```bash
KEEP_DEVNET_DATA=1 ./scripts/e2e-devnet.sh
```

This uses `data/e2e-devnet/` by default, starts a fresh network, checks transfers
and a WASM counter through different RPC nodes, checks parent quorum certificate
counts, stops validator 4, verifies progress with three validators, restarts it,
and compares a common finalized block on validators 1 and 4. It stops its
processes on exit. Without `KEEP_DEVNET_DATA=1`, it also removes its data/logs.
Do not run it alongside a network using the same ports.

For manual recovery on a devnet you own:

```bash
./scripts/devnet.sh kill 4
./scripts/devnet.sh status
./scripts/devnet.sh restart 4
./scripts/devnet.sh status
./scripts/devnet.sh stop
```

## Single-node debugging

```bash
cargo run -p aether-node --bin aether-node
```

Without `AETHER_GENESIS_PATH`, the binary constructs a single-validator network.
Defaults are data `./data/node1`, RPC 8545, P2P 9000, and metrics 9090. It loads or
generates a validator key at `data/node1/validator.key`. Use the multi-validator
launcher for shared genesis, keys, and funded development accounts.

If startup fails, check writable state/key paths, port collisions, and logs.
Review `AETHER_CONFIG_PATH`, `AETHER_NETWORK`, `AETHER_GENESIS_PATH`,
`AETHER_VALIDATOR_KEY`, `AETHER_NODE_DB_PATH`, `AETHER_RPC_PORT`,
`AETHER_P2P_PORT`, `AETHER_METRICS_PORT`, and `AETHER_BOOTSTRAP_PEERS` against
[node startup](../../crates/node/src/main.rs). If finality stalls, inspect peer
connections, matching genesis/validator membership, and every node's event log.

## Optional Compose and deployment assets

`docker-compose.test.yml` defines genesis initialization, four validators, and a
runner. `./scripts/docker-test.sh` builds/starts it, runs the runner, and tears it
down on success. The runner calls `./cli-test --in-process-only`: its scenarios
use in-memory transport and do not consume the Compose validators' `RPC_URL`.
A passing runner therefore does not qualify the container network journey.
Inspect and clean up containers after failures; the helper has no exit trap.

```bash
docker compose -f docker-compose.test.yml ps
docker compose -f docker-compose.test.yml logs --tail=200
docker compose -f docker-compose.test.yml down
```

`deploy/docker/docker-compose.yml` additionally declares RPC, PostgreSQL,
indexer, Prometheus, Grafana, and MinIO services. The presence of a service
manifest does not establish compatible service wiring or readiness. Compose,
Helm, Kubernetes, and Terraform assets require their own operator-reviewed
journeys. [CI](../../.github/workflows/ci.yml) builds an image but does not bring
up Compose, publish images, or deploy remote environments.

## Reset development state

Stop the relevant processes and preserve any keys/logs you need before deleting
state. `./scripts/devnet.sh clean` stops its recorded processes and deletes the
selected devnet directory. For Compose, `docker compose -f
docker-compose.test.yml down -v` also deletes its named data volumes. Avoid
`make clean` for a simple Rust rebuild: it also removes development Compose
volumes. Never reset another contributor's running network.
