# Getting started with Arrha / Aether

Requires Rust 1.86 or newer, Git, and Python 3 for the devnet E2E script.
Node.js 20+ and npm are needed for the web workspace build. Docker is optional.

## Build and run

```bash
git clone https://github.com/jadenfix/arrha.git
cd arrha
cargo build --workspace
cargo run -p aether-node --bin aether-node
```

The single-validator default uses the devnet preset, data under `data/node1`,
JSON-RPC on `127.0.0.1:8545`, P2P TCP on port 9000, and metrics on port 9090.
A validator key is generated if absent. Stop with Ctrl-C.

```bash
curl -sf http://127.0.0.1:8545/health
```

## Four-validator local network

```bash
./scripts/devnet.sh start
./scripts/devnet.sh status
./scripts/devnet.sh kill 3
./scripts/devnet.sh restart 3
./scripts/devnet.sh stop
```

The launcher builds the node and genesis ceremony, creates a common genesis,
and starts four processes. RPC: 8545–8548; P2P: 9000–9003; metrics: 9100–9103.
Logs and state live under `data/devnet`. `./scripts/devnet.sh clean` stops the
network and deletes its local state; preserve anything needed before running it.

## End-to-end tests and static checks

```bash
./cli-test                    # in-process E2E + real four-process devnet journey
./cli-test --in-process-only  # multi-node and agent-settlement library journeys
./scripts/lint.sh            # formatting, Clippy, workspace check
npm ci
npm run build:web            # SDK and UI TypeScript builds
```

The test suite intentionally contains only E2E scenarios. Unit, property,
doctest execution, and JS/Python unit-test runners were removed. Benchmark and
fuzz tools remain separate developer tools. See [testing](docs/testing.md).

## CLI and SDKs

```bash
cargo run -p aether-cli --bin aetherctl -- chain --help
cargo run -p aether-cli --bin aetherctl -- chain info
```

Use the README's transfer/deploy/call examples for the supported signed chain
journey. The Rust SDK has matching chain helpers. TypeScript/Python job helpers
and UI previews do not establish a deployed AI settlement service.

## Node configuration

| Variable | Purpose |
| --- | --- |
| `AETHER_NETWORK` | Built-in devnet/testnet/mainnet parameter preset; not proof of a public network. |
| `AETHER_CONFIG_PATH` | Chain TOML configuration override. |
| `AETHER_NODE_DB_PATH` | Node database directory. |
| `AETHER_VALIDATOR_KEY` | Validator key path. |
| `AETHER_GENESIS_PATH` | Shared multi-validator genesis JSON. |
| `AETHER_RPC_PORT` | JSON-RPC port. |
| `AETHER_RPC_RATE_LIMIT` | RPC per-second rate limit. |
| `AETHER_P2P_PORT` | P2P TCP port. |
| `AETHER_METRICS_PORT` | Prometheus exporter port. |
| `AETHER_BOOTSTRAP_PEERS` | Comma-separated libp2p peer addresses. |
| `AETHER_EVENT_LOG` | Consensus event log path. |

Read [architecture](docs/architecture.md), [contributing](CONTRIBUTING.md), and
[operations](docs/ops/RUNBOOKS.md) next. Compose/Helm/Kubernetes/Terraform files
are separate deployment assets; the supported local E2E path uses processes.
