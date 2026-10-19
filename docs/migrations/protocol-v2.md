# Protocol v2 migration and state compatibility

Protocol v2 changes consensus committee semantics and block execution. The
genesis committee remains fixed for consensus membership across epochs.
Economic slashing changes stake but does not remove a genesis validator from
the consensus committee. Committee reconfiguration requires a future finalized
protocol transition and is not currently supported.

Valid double-sign observations are queued as evidence for a locally produced
block. The presence of a locally detected offense, a queued proof, or a
speculatively executed block is not an economic slash: evidence is verified
against the registered validator BLS key, applied on the candidate branch, and
becomes canonical only when that block finalizes. Consensus membership and its
epoch voting snapshot stay fixed to genesis. Downtime slash proofs are rejected
until the protocol has on-chain slot-participation records; do not describe the
staking crate's downtime rate as an active node penalty. Surround-vote proofs are
also rejected because the current wire format lacks source/target ranges. Only
supported proofs passing verification and the finalized block path can slash.

Epoch reward credits and completed unbondings are evaluated as branch-local
block effects. A block crossing one or more epoch boundaries includes the
corresponding effects in its pending overlay; they become durable only when the
block finalizes. Transaction execution, credits, successful-transaction fee
distribution, state root, and receipt state roots follow the same block
execution path. Nodes on a partition or restart must not independently apply
epoch balance changes from local ticks.

Block IDs also commit to slash evidence. V2 hashes a domain tag plus the
length-prefixed bincode encodings of the header and evidence list. The parent
QC remains outside this commitment. Removing or changing evidence changes the
block ID, so an existing certificate cannot authorize different staking effects.

## Compatibility rules

- Genesis files must use the current `protocol_version` (or omit the field only
  if the loader's default is the current version). Older explicit versions are
  rejected during genesis validation. `genesis.json` is the shared ceremony
  artifact with matching validator keys; `genesis.toml` is a separate optional
  runtime chain-configuration override, not the validator key ceremony.
- A database with a protocol marker for a different version is rejected. A
  legacy database containing finalized block data without the marker is also
  rejected. Do not delete the marker or edit it to force startup.
- The node binds the database to the full runtime and genesis identity,
  including validator cryptographic keys and account parameters. Reopening a
  database under another genesis or chain configuration is rejected.
- The indexer separately binds RocksDB metadata to chain ID, genesis hash, and
  full runtime-plus-genesis chain identity, then checks its latest persisted
  block hash against the source node before resuming. An old nonempty index
  without identity metadata is rejected.

## Operator migration procedure

There is no in-place state conversion for prior protocol databases. To create a
new protocol-v2 network, coordinate a maintenance window and:

1. Preserve backups of the old node databases, index database, logs, keys, and
   old genesis artifact. Keep backups offline and do not reuse them as live
   state for the new network.
2. Produce a v2 genesis and validator keys from one ceremony; verify all
   validators have byte-identical genesis and the intended chain identity.
3. Start validators with fresh node database directories and validate matching
   `aeth_getNodeInfo` values (`chainId`, `genesisHash`, `chainIdentity`, and
   `protocolVersion`) before accepting clients.
4. Start the indexer with a fresh database path. Do not carry forward an index
   directory without matching identity metadata.
5. Run the four-validator transfer, finality, stop/rejoin, and slash-evidence
   journeys, plus the indexer persistence/restart journey against the new
   network before moving clients. Confirm an economic slash only from a
   finalized block carrying valid evidence. These local journeys do not qualify
   WAN behavior or a live cluster.

If startup rejects an old database, preserve it for forensic or offline
historical access. Recovery/conversion of historical state is not supplied by
the current node. Do not point production traffic at a new genesis until the
operator has separately decided how application balances and history are
handled.
