#!/usr/bin/env bash
# ============================================================================
# Aether local devnet: N validators (default 4) as separate processes.
# ============================================================================
#
#   ./scripts/devnet.sh start        build (release), run the genesis ceremony,
#                                    start validators, wait until blocks finalize
#   ./scripts/devnet.sh status       finalized slot / peers / mempool per node
#   ./scripts/devnet.sh kill <i>     stop validator i (1-based), keep its data
#   ./scripts/devnet.sh restart <i>  start validator i again (rejoins + catches up)
#   ./scripts/devnet.sh stop         stop all validators
#   ./scripts/devnet.sh clean        stop and delete all devnet data
#
# Environment:
#   VALIDATORS=4                     number of validators (1..4)
#   DEVNET_DIR=./data/devnet         keys, genesis, RocksDB dirs, logs
#   AETHER_BIN_DIR=./target/release  where aether-node / genesis-ceremony live
#   SKIP_BUILD=1                     do not run cargo build
#   BASE_RPC_PORT=8545 BASE_P2P_PORT=9000 BASE_METRICS_PORT=9100
#
# Validator i listens on RPC $BASE_RPC_PORT+i-1, P2P $BASE_P2P_PORT+i-1 and
# metrics $BASE_METRICS_PORT+i-1, dials every other validator, and writes
# logs to $DEVNET_DIR/node<i>.log and consensus events to
# $DEVNET_DIR/node<i>.events.jsonl.
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VALIDATORS="${VALIDATORS:-4}"
DEVNET_DIR="${DEVNET_DIR:-$ROOT/data/devnet}"
BIN_DIR="${AETHER_BIN_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}/release}"
BASE_RPC_PORT="${BASE_RPC_PORT:-8545}"
BASE_P2P_PORT="${BASE_P2P_PORT:-9000}"
BASE_METRICS_PORT="${BASE_METRICS_PORT:-9100}"
GENESIS_DIR="$DEVNET_DIR/genesis"

if [ "$VALIDATORS" -lt 1 ] || [ "$VALIDATORS" -gt 4 ]; then
  echo "VALIDATORS must be between 1 and 4" >&2
  exit 1
fi

rpc() { # rpc <port> <method> [params-json]
  curl -s -m 3 -X POST "http://127.0.0.1:$1" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$2\",\"params\":${3:-[]},\"id\":1}"
}

pid_file() { echo "$DEVNET_DIR/node$1.pid"; }

start_node() {
  local i="$1"
  local rpc_port=$((BASE_RPC_PORT + i - 1))
  local p2p_port=$((BASE_P2P_PORT + i - 1))
  local metrics_port=$((BASE_METRICS_PORT + i - 1))
  local peers=""
  for j in $(seq 1 "$VALIDATORS"); do
    [ "$j" = "$i" ] && continue
    peers="${peers:+$peers,}/ip4/127.0.0.1/tcp/$((BASE_P2P_PORT + j - 1))"
  done
  mkdir -p "$DEVNET_DIR/node$i"
  RUST_LOG="${RUST_LOG:-info}" \
  AETHER_GENESIS_PATH="$GENESIS_DIR/genesis.json" \
  AETHER_VALIDATOR_KEY="$GENESIS_DIR/validator-$i.key" \
  AETHER_NODE_DB_PATH="$DEVNET_DIR/node$i" \
  AETHER_RPC_PORT="$rpc_port" \
  AETHER_P2P_PORT="$p2p_port" \
  AETHER_METRICS_PORT="$metrics_port" \
  AETHER_BOOTSTRAP_PEERS="$peers" \
  AETHER_EVENT_LOG="$DEVNET_DIR/node$i.events.jsonl" \
  AETHER_RPC_RATE_LIMIT="${AETHER_RPC_RATE_LIMIT:-2000}" \
    "$BIN_DIR/aether-node" >> "$DEVNET_DIR/node$i.log" 2>&1 &
  echo $! > "$(pid_file "$i")"
  echo "  validator $i  pid $!  rpc :$rpc_port  p2p :$p2p_port  metrics :$metrics_port"
}

stop_node() {
  local f; f="$(pid_file "$1")"
  if [ -f "$f" ]; then
    local pid; pid="$(cat "$f")"
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$f"
  fi
}

stop_all() {
  for i in 1 2 3 4; do stop_node "$i"; done
}

status() {
  for i in $(seq 1 "$VALIDATORS"); do
    local port=$((BASE_RPC_PORT + i - 1))
    printf 'validator %s: ' "$i"
    rpc "$port" aeth_getNodeInfo || printf 'down'
    echo
  done
}

wait_for_finality() { # wait until every running node reports finalizedSlot > 0
  local deadline=$((SECONDS + ${1:-60}))
  while [ $SECONDS -lt $deadline ]; do
    local ok=1
    for i in $(seq 1 "$VALIDATORS"); do
      local fs
      fs=$(rpc $((BASE_RPC_PORT + i - 1)) aeth_getFinalizedSlot | sed -n 's/.*"result":\([0-9]*\).*/\1/p')
      if [ -z "$fs" ] || [ "$fs" -eq 0 ]; then ok=0; fi
    done
    if [ $ok = 1 ]; then return 0; fi
    sleep 0.5
  done
  return 1
}

cmd="${1:-start}"
case "$cmd" in
  start)
    if [ -z "${SKIP_BUILD:-}" ]; then
      echo "Building aether-node (release)..."
      (cd "$ROOT" && cargo build --release -p aether-node --bins)
    fi
    stop_all
    rm -rf "$DEVNET_DIR"
    mkdir -p "$DEVNET_DIR"
    "$BIN_DIR/genesis-ceremony" --validators "$VALIDATORS" --output-dir "$GENESIS_DIR" \
      --genesis-delay-ms "${GENESIS_DELAY_MS:-3000}" > "$DEVNET_DIR/ceremony.log"
    echo "Genesis: $GENESIS_DIR/genesis.json ($VALIDATORS validators, 256 funded devnet accounts)"
    for i in $(seq 1 "$VALIDATORS"); do start_node "$i"; done
    echo "Waiting for finality..."
    if wait_for_finality 90; then
      echo "Devnet is finalizing blocks."
      status
    else
      echo "Devnet did not finalize within 90s; see $DEVNET_DIR/node*.log" >&2
      exit 1
    fi
    ;;
  status) status ;;
  kill) stop_node "${2:?validator index}"; echo "validator $2 stopped" ;;
  restart) start_node "${2:?validator index}" ;;
  stop) stop_all; echo "All validators stopped." ;;
  clean) stop_all; rm -rf "$DEVNET_DIR"; echo "Devnet data removed." ;;
  *) echo "usage: $0 {start|status|kill <i>|restart <i>|stop|clean}" >&2; exit 1 ;;
esac
