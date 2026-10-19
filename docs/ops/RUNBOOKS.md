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

`docker-compose.test.yml` defines one genesis initializer, four validators on a
shared network, and a runner that calls the validators' real JSON-RPC endpoints.
It waits for finality and peer connectivity, submits a signed transfer through
validator 1, then checks the finalized balance through validator 4.
`./scripts/docker-test.sh` builds and runs this journey and removes its project
and named volumes on exit. The CI Docker job runs the same Compose journey.

```bash
docker compose -f docker-compose.test.yml ps
docker compose -f docker-compose.test.yml logs --tail=200
docker compose -f docker-compose.test.yml down
```

`deploy/docker/docker-compose.yml` includes the canonical Compose journey; it
does not add separate RPC, database, indexer, Prometheus, Grafana, or MinIO
services. CI builds the node image and runs the container journey, but does not
publish images or deploy remote environments.

The Kubernetes and Helm validator templates require an operator-provided
`genesis.json`, validator key files from the same ceremony, a suitable storage
class, and an operator-built image. `/health` is the process liveness probe;
`/ready` remains unavailable until the node has finalized a block and is not
syncing. These templates have not been qualified against a live cluster.
The pod entrypoint derives bootstrap peers from `POD_NAME`, `POD_NAMESPACE`,
`NUM_VALIDATORS`, `AETHER_HEADLESS_SERVICE`, and `AETHER_P2P_PORT` when
`AETHER_BOOTSTRAP_PEERS` is empty. The headless service name is an explicit
operator value (the Helm chart default is `aether-validator`); a nonempty
`AETHER_BOOTSTRAP_PEERS` value overrides derivation. The Compose journey covers
node-to-node service-name DNS, but does not execute the Kubernetes ordinal
derivation or qualify cluster networking.

Node configuration precedence is explicit `AETHER_CONFIG_PATH`, then an
explicit `AETHER_NETWORK` preset, then the chain configuration embedded in the
validated genesis file, with the devnet preset used only when no genesis is
provided. A custom genesis should normally be the source of both genesis and
chain settings; setting `AETHER_CONFIG_PATH` intentionally overrides its chain
settings. `chainIdentity` covers the full runtime configuration and genesis
parameters, including validator cryptographic keys and initial accounts. See
[protocol v2 migration](../migrations/protocol-v2.md) before
reusing existing state or changing genesis.

## Run the standalone indexer

The indexer is a separate process. Point it at a reachable node RPC endpoint
and give it a persistent RocksDB path:

```bash
AETHER_RPC_URL=http://127.0.0.1:8545 \
INDEXER_DB_PATH=./data/indexer \
INDEXER_BIND_IP=127.0.0.1 \
INDEXER_PORT=8081 \
./target/release/aether-indexer
```

It indexes finalized blocks only. `GET /status` returns 503 while starting,
catching up, or retrying a source error, and returns 200 when caught up. Before
resuming and on each RPC poll, it checks the stored chain ID, genesis hash,
chain configuration identity, and latest indexed block hash against the node.
An old database with blocks but no identity metadata is rejected; use a fresh
`INDEXER_DB_PATH` to rebuild it. `GET /block/{slot}` returns a persisted block
summary or 404 when that slot has no indexed block.

With a live four-validator devnet and a built indexer binary, run the real
restart journey while the node is running:

```bash
AETHER_INDEXER_BIN=target/release/aether-indexer \
AETHER_RPC_URL=http://127.0.0.1:8545 \
python3 scripts/indexer-restart-e2e.py
```

It checks finalized ingestion, reports the chain identity, stops and restarts
the indexer on the same database, and verifies the persisted checkpoint.

## Reset development state

Stop the relevant processes and preserve any keys/logs you need before deleting
state. `./scripts/devnet.sh clean` stops its recorded processes and deletes the
selected devnet directory. For Compose, `docker compose -f
docker-compose.test.yml down -v` also deletes its named data volumes. Avoid
`make clean` for a simple Rust rebuild: it also removes development Compose
volumes. Never reset another contributor's running network.
