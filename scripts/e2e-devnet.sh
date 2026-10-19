#!/usr/bin/env bash
# ============================================================================
# End-to-end devnet test (run in CI and locally).
#
# Starts 4 validator processes with ./scripts/devnet.sh and asserts that:
#   1. all validators finalize blocks and agree on the finalized block hash
#   2. a new account can be created and funded (transfer finalizes)
#   3. the new account can send a transfer of its own
#   4. a WASM counter contract deploys and two calls update its storage to 7
#      (1 + 6), visible from a different validator's RPC
#   5. finalized blocks carry quorum certificates signed by >= 3 of 4
#   6. with validator 4 killed (f = 1) the other 3 keep finalizing, and after
#      a restart validator 4 catches up to the same finalized chain
#
# Usage: ./scripts/e2e-devnet.sh   (SKIP_BUILD=1 / AETHER_BIN_DIR to reuse binaries)
# ============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export DEVNET_DIR="${DEVNET_DIR:-$ROOT/data/e2e-devnet}"
export VALIDATORS=4
export DEVNET_DNS_HOST="${DEVNET_DNS_HOST:-localhost}"
BIN_DIR="${AETHER_BIN_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}/release}"
export AETHER_BIN_DIR="$BIN_DIR"
CTL="$BIN_DIR/aetherctl"
RPC1=http://127.0.0.1:8545
RPC3=http://127.0.0.1:8547

if [ -z "${SKIP_BUILD:-}" ]; then
  (cd "$ROOT" && cargo build --release -p aether-node -p aether-cli --bins)
fi
export SKIP_BUILD=1

cleanup() {
  if ! "$ROOT/scripts/devnet.sh" stop; then
    echo "Cleanup could not verify all process owners; preserving $DEVNET_DIR" >&2
    return 1
  fi
  if [ -z "${KEEP_DEVNET_DATA:-}" ]; then rm -rf "$DEVNET_DIR"; fi
}
trap cleanup EXIT

fail() {
  echo "E2E FAILED: $*" >&2
  for i in 1 2 3 4; do
    echo "---- tail node$i.log" >&2
    tail -n 30 "$DEVNET_DIR/node$i.log" >&2 2>/dev/null || true
  done
  exit 1
}

jget() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval('d'+sys.argv[1]))" "$1"; }
rpc() {
  curl -s -m 3 -X POST "$1" -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"method\":\"$2\",\"params\":${3:-[]},\"id\":1}"
}
finalized_slot() { rpc "$1" aeth_getNodeInfo | jget "['result']['finalizedSlot']"; }
finalized_hash() { rpc "$1" aeth_getNodeInfo | jget "['result']['finalizedHash']"; }

echo "== 1. start 4 validators"
"$ROOT/scripts/devnet.sh" start || fail "devnet did not start"

echo "== 2. create + fund an account"
KEY="$DEVNET_DIR/alice.json"
"$CTL" keys generate --out "$KEY" > "$DEVNET_DIR/alice.out"
ALICE=$(jget "['address']" < "$DEVNET_DIR/alice.out")
"$CTL" chain --rpc "$RPC1" transfer --dev 0 --to "$ALICE" --amount 5000000000 --wait > "$DEVNET_DIR/t1.json" \
  || fail "funding transfer"
BAL=$("$CTL" chain --rpc "$RPC3" account "$ALICE" | jget "['finalized']['balance']")
[ "$BAL" = "5000000000" ] || fail "alice balance on validator 3 is $BAL"
echo "   alice $ALICE balance $BAL (read from validator 3)"

echo "== 3. transfer from the new account"
"$CTL" chain --rpc "$RPC3" transfer --key "$KEY" --to dev:5 --amount 1000 --wait > /dev/null \
  || fail "transfer from alice"

echo "== 4. deploy + call the counter contract"
"$CTL" chain --rpc "$RPC1" deploy --key "$KEY" "$ROOT/examples/contracts/counter.wat" --wait \
  > "$DEVNET_DIR/deploy.json" || fail "deploy"
CONTRACT=$(jget "['contract']" < "$DEVNET_DIR/deploy.json")
"$CTL" chain --rpc "$RPC1" call --key "$KEY" --contract "$CONTRACT" --wait > /dev/null || fail "call 1"
"$CTL" chain --rpc "$RPC1" call --key "$KEY" --contract "$CONTRACT" --u64 6 --wait > "$DEVNET_DIR/call2.json" \
  || fail "call 2"
VALUE=$("$CTL" chain --rpc "$RPC3" storage "$CONTRACT" | python3 -c "
import json,sys; d=json.load(sys.stdin); v=d.get('0x636f756e74')  # 'count'
print(int.from_bytes(bytes.fromhex(v[2:]),'little') if v else -1)")
[ "$VALUE" = "7" ] || fail "counter value on validator 3 is $VALUE, expected 7"
echo "   counter $CONTRACT = $VALUE (read from validator 3)"

echo "== 5. finalized blocks carry quorum certificates"
"$CTL" chain --rpc "$RPC1" blocks --last 40 | tee "$DEVNET_DIR/blocks.txt" | tail -n 8
grep -q "FINAL" "$DEVNET_DIR/blocks.txt" || fail "no finalized blocks in explorer view"
grep -Eq "parent-QC [34]/4" "$DEVNET_DIR/blocks.txt" || fail "no block with a 3/4 or 4/4 QC"

echo "== 6. kill validator 4 (f=1), keep finalizing, restart and catch up"
"$ROOT/scripts/devnet.sh" kill 4
BEFORE=$(finalized_slot "$RPC1")
sleep 15
AFTER=$(finalized_slot "$RPC1")
[ "$AFTER" -gt "$BEFORE" ] || fail "no finality progress with 3/4 validators ($BEFORE -> $AFTER)"
echo "   with 3/4 validators finalized slot $BEFORE -> $AFTER"
"$ROOT/scripts/devnet.sh" restart 4 > /dev/null
DEADLINE=$((SECONDS + 60))
while :; do
  F4=$(finalized_slot http://127.0.0.1:8548 2>/dev/null || echo 0)
  F1=$(finalized_slot "$RPC1")
  if [ "${F4:-0}" -ge "$AFTER" ]; then break; fi
  [ $SECONDS -lt $DEADLINE ] || fail "validator 4 did not catch up (at $F4, network at $F1)"
  sleep 1
done
echo "   validator 4 rejoined and reached finalized slot $F4 (network at $F1)"

echo "== agreement: same finalized block at a common slot"
sleep 3
S=$(finalized_slot http://127.0.0.1:8548)
H1=$(rpc "$RPC1" aeth_getBlockSummaries "[$S,$S]" | jget "['result'][0]['hash']")
H4=$(rpc http://127.0.0.1:8548 aeth_getBlockSummaries "[$S,$S]" | jget "['result'][0]['hash']")
[ "$H1" = "$H4" ] || fail "validators 1 and 4 disagree at slot $S: $H1 vs $H4"
echo "   slot $S: $H1 on validators 1 and 4"

if [ "${RUN_CLIENT_E2E:-0}" = "1" ]; then
  echo "== 9. TypeScript and Python signed transfers across validators"
  export AETHERCTL_BIN="$CTL"
  export AETHER_RPC_FROM="$RPC1"
  export AETHER_RPC_READ="$RPC3"
  node "$ROOT/scripts/client-e2e.mjs" || fail "TypeScript signed transfer journey"
  "${AETHER_PYTHON_BIN:-python3}" "$ROOT/scripts/client-e2e.py" || fail "Python signed transfer journey"
fi

if [ "${RUN_INDEXER_E2E:-0}" = "1" ]; then
  echo "== 10. persistent indexer finalized-chain restart"
  "${AETHER_PYTHON_BIN:-python3}" "$ROOT/scripts/indexer-restart-e2e.py" || fail "persistent indexer restart journey"
fi

echo "E2E PASSED"
