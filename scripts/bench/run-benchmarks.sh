#!/usr/bin/env bash
# ============================================================================
# Reproducible single-machine benchmarks for the 4-validator devnet.
#
#   ./scripts/bench/run-benchmarks.sh            # all scenarios
#   SCENARIOS="transfer liveness" ./scripts/bench/run-benchmarks.sh
#
# Every scenario starts a FRESH devnet (new genesis, empty RocksDB) so runs do
# not affect each other. Results (JSON) go to $OUT_DIR (default bench/results).
# Scenarios:
#   transfer  - offered load sweep of simple transfers
#   contract  - offered load sweep of WASM counter-contract calls
#   idle      - 60 s with no load: block interval and empty-block finality
#   liveness  - 100 tx/s for 90 s; validator 3 killed at t=25 s and restarted
#               at t=55 s; consensus event logs are kept for the timeline
#   bls       - BLS signature verification: single vs batch vs aggregate
# ============================================================================
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/bench/results}"
BIN_DIR="${AETHER_BIN_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}/release}"
export AETHER_BIN_DIR="$BIN_DIR" SKIP_BUILD=1
export DEVNET_DIR="${DEVNET_DIR:-$ROOT/data/bench-devnet}"
SCENARIOS="${SCENARIOS:-bls idle transfer contract liveness}"
TRANSFER_RATES="${TRANSFER_RATES:-100 250 400 550}"
CONTRACT_RATES="${CONTRACT_RATES:-25 50 100 150}"
DURATION="${DURATION:-40}"
RPCS=http://127.0.0.1:8545,http://127.0.0.1:8546,http://127.0.0.1:8547,http://127.0.0.1:8548
LOADGEN="$BIN_DIR/aether-loadgen"
mkdir -p "$OUT_DIR"

fresh() { "$ROOT/scripts/devnet.sh" clean >/dev/null; "$ROOT/scripts/devnet.sh" start >/dev/null; }
trap '"$ROOT/scripts/devnet.sh" stop >/dev/null 2>&1 || true' EXIT

for s in $SCENARIOS; do
  case "$s" in
    bls)
      echo "== BLS verification"
      "$LOADGEN" bls --count 1024 > "$OUT_DIR/bls.json"
      ;;
    idle)
      echo "== idle network, 60 s"
      fresh; sleep 60
      cp "$DEVNET_DIR"/node1.events.jsonl "$OUT_DIR/idle-node1.events.jsonl"
      ;;
    transfer|contract)
      rates=$TRANSFER_RATES; [ "$s" = contract ] && rates=$CONTRACT_RATES
      for r in $rates; do
        echo "== $s @ $r tx/s for ${DURATION}s"
        fresh
        "$LOADGEN" run --rpc "$RPCS" --mode "$s" --rate "$r" --duration "$DURATION" \
          --warmup 10 --drain 45 --out "$OUT_DIR/$s-$r.json" > /dev/null
        python3 -c "import json;d=json.load(open('$OUT_DIR/$s-$r.json'));print('   finalized %.0f tx/s, p50 %d ms, p95 %d ms, p99 %d ms, lost %d' % (d['finalizedTpsSteadyWindow'], d['finalityMs']['p50'], d['finalityMs']['p95'], d['finalityMs']['p99'], d['notFinalizedAfterDrain']))"
      done
      ;;
    liveness)
      echo "== liveness: kill validator 3 at t=25s, restart at t=55s, 100 tx/s"
      fresh
      "$LOADGEN" run --rpc http://127.0.0.1:8545,http://127.0.0.1:8546,http://127.0.0.1:8548 \
        --mode transfer --rate 100 --duration 90 --warmup 5 --drain 45 \
        --out "$OUT_DIR/liveness-load.json" > /dev/null &
      LG=$!
      START=$(python3 -c 'import time;print(int(time.time()*1000))')
      sleep 25; KILL=$(python3 -c 'import time;print(int(time.time()*1000))')
      "$ROOT/scripts/devnet.sh" kill 3
      sleep 30; RESTART=$(python3 -c 'import time;print(int(time.time()*1000))')
      "$ROOT/scripts/devnet.sh" restart 3 >/dev/null
      wait $LG
      echo "{\"startMs\":$START,\"killMs\":$KILL,\"restartMs\":$RESTART,\"killedValidator\":3}" > "$OUT_DIR/liveness-schedule.json"
      mkdir -p "$OUT_DIR/liveness-events"
      cp "$DEVNET_DIR"/node*.events.jsonl "$OUT_DIR/liveness-events/"
      cp "$DEVNET_DIR"/genesis/genesis.json "$OUT_DIR/liveness-events/genesis.json"
      ;;
    *) echo "unknown scenario $s" >&2; exit 1 ;;
  esac
done
"$ROOT/scripts/devnet.sh" clean >/dev/null
echo "results in $OUT_DIR"
