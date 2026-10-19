# Getting started with Arrha / Aether

Use current stable Rust with Cargo, Git, Bash, curl, and Python 3 for the devnet
scripts. The workspace package metadata declares Rust 1.86; CI installs stable.
Native RocksDB bindings need a C/C++ toolchain and libclang. On macOS, install the
Xcode Command Line Tools (`xcode-select --install`) and make libclang available;
on Debian/Ubuntu, the Docker build uses `pkg-config clang llvm-dev libclang-dev`.
Node.js 20+ and npm are needed for the web workspace build. Docker is optional.

Run the commands below from the repository root. Arrha is the repository name;
packages and executable names retain the Aether name.

## Build and run

```bash
git clone https://github.com/jadenfix/arrha.git
cd arrha
cargo build --workspace
cargo run -p aether-node --bin aether-node
```

This starts one node in the foreground; use a second terminal for CLI/RPC commands.
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

Stop the single node before starting the four-validator launcher: both use RPC
port 8545 and P2P port 9000 by default. **Every `start` deletes the managed devnet's
previous data**, so use `restart <i>` to rejoin an individual stopped validator.

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

The full E2E command builds release node/CLI binaries and starts four processes
on RPC ports 8545–8548. Stop other networks using those ports first. It uses and
removes `data/e2e-devnet`; set `KEEP_DEVNET_DATA=1` to retain artifacts. The shorter
in-process command does not bind those ports or prove real networking.

The test suite intentionally contains only E2E scenarios. Unit, property,
doctest execution, and JS/Python unit-test runners were removed. Benchmark and
fuzz tools remain separate developer tools. See [testing](docs/testing.md).

## CLI and SDKs

The help command works offline. `chain info` requires a running node. If you
stopped the devnet or ran the full E2E command above, start a fresh devnet first
with `./scripts/devnet.sh start` (which resets its managed data as described above).

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
| `AETHER_RPC_RATE_LIMIT` | RPC per-second refill rate; node default 50, devnet launcher default 2,000. |
| `AETHER_P2P_PORT` | P2P TCP port. |
| `AETHER_METRICS_PORT` | Prometheus exporter port; 9090 for a standalone node, 9100–9103 in the launcher. |
| `AETHER_BOOTSTRAP_PEERS` | Comma-separated libp2p peer addresses. |
| `AETHER_EVENT_LOG` | Consensus event log path. |

Read [architecture](docs/architecture.md), [contributing](CONTRIBUTING.md), and
[operations](docs/ops/RUNBOOKS.md) next. Compose/Helm/Kubernetes/Terraform files
are separate deployment assets; the supported local E2E path uses processes.
