# Executable settlement indexing

The indexer now connects to `INDEXER_RPC_URL` (default
`http://127.0.0.1:8545`) and reads canonical finalized blocks and transaction
receipts from the real node. It no longer subscribes to its own empty local
firehose. `INDEXER_DB_PATH` selects its persistent RocksDB database.

For each finalized slot it validates receipt transaction hashes, block hashes,
and slots, and commits the block, transaction lookup, receipts and next-slot
cursor in one batch. Missing receipts or RPC errors leave the cursor unchanged.
Legitimate empty slots below the finalized head advance the cursor. A missing
finalized-head block is retried. The chain ID is pinned in the database;
using another independent devnet with the same chain ID requires a new database.

The loopback query API serves `/status`, `/block/{slot}`, and `/tx/{hash}`.
Unknown records return HTTP 404, malformed transaction hashes HTTP 400, and
storage failures HTTP 500. Status includes synchronization errors rather than
reporting an unavailable node as successfully synced.

Run the actual signed-transfer workflow:

```sh
cargo test -p aether-indexer -p aether-sdk
cargo build -p aether-node -p aether-cli -p aether-indexer --bin aether-node --bin aetherctl --bin aether-indexer
python3 scripts/settlement-e2e.py
```

This starts an ephemeral devnet, generates throwaway keys, requests development
funds, submits a signed transaction through the real CLI, waits for its execution
receipt, reconciles indexed records and recipient balance, and restarts both
indexer and node to check durable state. It records the tested commit and tears
down its child processes. It never calls a funded public network or model provider.

This establishes a native transaction-to-receipt-to-indexer path. It does not
establish public-network safety, distributed finality, full agent escrow/dispute
bridges, reorg recovery, or indexer resilience against a malicious RPC node.
