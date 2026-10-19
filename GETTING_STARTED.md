# Getting started with Arrha / Aether

Use current stable Rust with Cargo, Git, Bash, curl, and Python 3 for the devnet
scripts. The workspace package metadata declares Rust 1.86; CI installs stable.
Native RocksDB bindings need a C/C++ toolchain and libclang. On macOS, install the
Xcode Command Line Tools (`xcode-select --install`) and make libclang available;
on Debian/Ubuntu, the Docker build uses `pkg-config clang llvm-dev libclang-dev`.
Node.js 20+ and npm are needed for the TypeScript SDK and browser apps. Docker is optional.

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
journey. Rust SDK online submission validates against the node's live fee parameters;
use `chain::*_with_fee_parameters` or `TransferBuilder::build_with_fee_parameters`
for custom chains. The original synchronous `chain::transfer_tx`, `deploy_tx`,
`call_tx`, and `finalize_tx` helpers retain devnet fee defaults; the legacy
`TransferBuilder::build` uses its configured default fee and devnet fee validation.
Online `AetherClient::submit` fetches live parameters and rejects a fee or chain-ID
mismatch without changing the signed transaction. TypeScript/Python job helpers
and UI previews do not establish a deployed AI settlement service.

### TypeScript SDK and browser apps

From the repository root, install dependencies and build the SDK and both apps:

```bash
npm ci
npm run build:web
```

Start either app with a validator reachable at `http://127.0.0.1:8545`:

```bash
npm run dev:explorer  # http://127.0.0.1:3000
npm run dev:wallet    # in another terminal, http://127.0.0.1:3001
```

Override the RPC endpoint by setting `VITE_AETHER_RPC_ENDPOINT` in the app
workspace environment before starting Vite. Both apps make browser JSON-RPC
requests, so the node must allow the corresponding local page origins. If RPC
cannot be reached, the explorer labels the live values unavailable and the wallet
shows an RPC error; unavailable is not a zero measurement. The wallet generates
or imports an Ed25519 seed in the current tab, signs transfers locally, and can
request a local faucet credit when the single-validator node enables it. That
credit is an immediate local state seed, not a transaction; faucet requests are
disabled on multi-validator devnets. Use the wallet's displayed CLI command to
fund its address on a multi-validator devnet. The seed is neither encrypted nor
persisted; use only with a local/devnet node and never enter a mainnet key.

### Python SDK and live SDK journeys

The Python wheel can be installed from the repository in an isolated environment:

```bash
python3 -m venv work/python-sdk-venv
work/python-sdk-venv/bin/python -m pip install --upgrade pip setuptools wheel
work/python-sdk-venv/bin/python -m pip install ./sdks/python
work/python-sdk-venv/bin/python -c 'import aether_sdk; print(aether_sdk.__file__)'
```

With the four-validator devnet running and `target/release/aetherctl` built, each
retained client journey funds a temporary key through the Rust CLI, submits the
SDK's canonical signed binary transaction to validator 1, waits for a successful
finalized receipt, and verifies the precise recipient balance delta through
validator 3:

```bash
npm run e2e:client
work/python-sdk-venv/bin/python scripts/client-e2e.py
```

The journeys default to RPC ports 8545 and 8547; use `AETHER_RPC_FROM`,
`AETHER_RPC_READ`, and `AETHERCTL_BIN` to select other endpoints or the CLI path.
They require funded devnet support and do not run against a hosted production
network. An offline SDK build or matching TS/Python serialization alone is not
Rust acceptance evidence. The scripts only report `PASS` after Rust accepts the
raw transaction and the second validator confirms finalized state.

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
