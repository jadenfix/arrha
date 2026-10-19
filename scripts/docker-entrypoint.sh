#!/bin/sh
# Start one validator from the shared genesis/key volume.
set -eu

GENESIS_DIR="${AETHER_GENESIS_DIR:-/app/data/genesis}"
GENESIS_PATH="${AETHER_GENESIS_PATH:-${GENESIS_DIR}/genesis.json}"

# StatefulSet ordinals are zero-based; genesis ceremony key files are one-based.
if [ -n "${POD_NAME:-}" ]; then
    ordinal="${POD_NAME##*-}"
    VALIDATOR_ID="${VALIDATOR_ID:-$((ordinal + 1))}"
else
    VALIDATOR_ID="${VALIDATOR_ID:-1}"
fi

ATTEMPTS=0
while [ ! -r "$GENESIS_PATH" ]; do
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -gt 30 ]; then
        echo "ERROR: genesis.json not readable after 30s at $GENESIS_PATH" >&2
        exit 1
    fi
    sleep 1
done

export AETHER_GENESIS_PATH="$GENESIS_PATH"
KEY_DIR="${AETHER_KEY_DIR:-$GENESIS_DIR}"
export AETHER_VALIDATOR_KEY="${AETHER_VALIDATOR_KEY:-${KEY_DIR}/validator-${VALIDATOR_ID}.key}"
export AETHER_NODE_DB_PATH="${AETHER_NODE_DB_PATH:-/app/data/validator-${VALIDATOR_ID}}"
export AETHER_RPC_BIND_IP="${AETHER_RPC_BIND_IP:-0.0.0.0}"

# StatefulSet pods need deterministic peer targets. Keep an explicit peer list
# authoritative; otherwise derive every other ordinal from the operator-provided
# headless service DNS name. Do not infer the service name from a Helm release.
if [ -z "${AETHER_BOOTSTRAP_PEERS:-}" ] && [ -n "${POD_NAME:-}" ]; then
    headless_service="${AETHER_HEADLESS_SERVICE:-}"
    pod_namespace="${POD_NAMESPACE:-}"
    if [ -n "$headless_service" ] && [ -n "$pod_namespace" ]; then
        case "$ordinal" in
            ''|*[!0-9]*) echo "ERROR: invalid StatefulSet ordinal in POD_NAME=$POD_NAME" >&2; exit 1 ;;
        esac
        validator_count="${NUM_VALIDATORS:-1}"
        case "$validator_count" in
            ''|*[!0-9]*|0) echo "ERROR: NUM_VALIDATORS must be a positive integer" >&2; exit 1 ;;
        esac
        p2p_port="${AETHER_P2P_PORT:-9000}"
        peers=""
        peer_ordinal=0
        while [ "$peer_ordinal" -lt "$validator_count" ]; do
            if [ "$peer_ordinal" -ne "$ordinal" ]; then
                peer="/dns4/${POD_NAME%-*}-${peer_ordinal}.${headless_service}.${pod_namespace}.svc.cluster.local/tcp/${p2p_port}"
                if [ -n "$peers" ]; then peers="${peers},${peer}"; else peers="$peer"; fi
            fi
            peer_ordinal=$((peer_ordinal + 1))
        done
        AETHER_BOOTSTRAP_PEERS="$peers"
    fi
fi
export AETHER_BOOTSTRAP_PEERS="${AETHER_BOOTSTRAP_PEERS:-}"

if [ ! -r "$AETHER_VALIDATOR_KEY" ]; then
    echo "ERROR: validator key not readable: $AETHER_VALIDATOR_KEY" >&2
    exit 1
fi

echo "Starting validator ${VALIDATOR_ID}..."
echo "  Genesis: $AETHER_GENESIS_PATH"
echo "  Key:     $AETHER_VALIDATOR_KEY"
echo "  DB:      $AETHER_NODE_DB_PATH"

exec aether-node "$@"
