# Arrha / Aether threat model

Reviewed against the repository on **2026-10-18**. This models the running
`aether-node` and separately identifies risks in reusable libraries and proposed
integrations. It records review priorities, not an external security audit.

## System and trust boundaries

The [architecture](../architecture.md) traces the assembled node. Clients submit
Ed25519-signed transactions through loopback JSON-RPC; validators exchange
messages through libp2p TCP with Noise, Yamux, and gossipsub. `HybridConsensus`
uses VRF leader eligibility and stake-weighted BLS certificates. Finality requires
a certified parent and certified child in consecutive slots; single-validator
mode has a separate immediate-finality path. Ledger execution is sequential,
with Wasmtime for contracts and RocksDB/sparse-Merkle persistence.

| Boundary | Input and security concern | Source |
| --- | --- | --- |
| Client → RPC/mempool | Untrusted bytes, signature/nonce/fee validation, rate limits and resource exhaustion | `crates/rpc/json-rpc`, `crates/mempool`, `crates/node/src/main.rs` |
| Peer → node | Transport identity is distinct from validator authority; decoded proposals/votes need protocol validation | `crates/p2p/src/network.rs`, `crates/node/src/network_handler.rs` |
| Proposal → ledger | Invalid signatures, roots, gas use, or parent-state selection must not become accepted state | `crates/node/src/node.rs`, `crates/ledger/src/state.rs` |
| Pending → finalized storage | Branch selection, crash atomicity, restart/catch-up and staking persistence | `crates/node/src/node.rs`, `crates/state/storage` |
| Contract → host | Malicious WASM, fuel/memory exhaustion, host-function authorization | `crates/runtime`, `crates/node/src/main.rs` |
| Operator → validator keys/config | Local signing secrets, deterministic development identities, genesis/validator membership | `crates/node/src/main.rs`, `crates/node/src/hybrid_node.rs` |

Default node RPC is `127.0.0.1:8545`; P2P listens on `0.0.0.0:9000` and
Prometheus on `0.0.0.0:9090`. Environment overrides and deployment port mappings
must be reviewed for the intended environment. Metrics exposure is a separate
operator decision from the loopback RPC default.

## STRIDE review

| Category / threat | Controls visible in source | Remaining review or operational requirement |
| --- | --- | --- |
| Spoofed transaction or validator vote | Transaction signature verification; BLS vote/QC checks; VRF eligibility checks | Audit key registration, PoP enforcement, signed domains, chain binding, duplicate signers and malformed encodings across every ingress path. Crypto helper availability alone proves none of these end-to-end. |
| Sybil peers / eclipse | P2P connection limits, per-peer rate limiting, reputation and bounded message sizes | An authenticated Noise peer is not automatically an authorized validator. Qualify isolation resistance, reconnect behavior and hostile peer topologies. |
| Tampered blocks or state | Re-execution, state/receipt root checks, hash-linked blocks, finalized atomic batches | Audit conflicting branches, gap slots, stale certificates, recovery metadata and snapshot trust. Hashes do not repair host compromise. |
| Repudiation / equivocation | Signed transactions and consensus messages provide evidence | Audit evidence retention and actual slashing dispatch; do not assume an insurance fund, dispute window or governance approval is active in the binary. |
| Private-key disclosure | Signing occurs locally in the node | Audit filesystem permissions, logs, memory and backups. HSM/KMS and remote signing are proposals, not an available node mode. |
| MEV / ordering manipulation | Mempool ordering and fee policy exist | No claim of fair ordering, encrypted mempool or MEV prevention. Review proposer discretion and economic assumptions. |
| RPC / network DoS | JSON-RPC rate limiter; bounded P2P messages/connections; mempool admission controls | Measure aggregate memory/CPU pressure, malformed inputs and cross-peer abuse. Limits are not adversarial capacity measurements. |
| State / contract exhaustion | Transaction gas and runtime fuel/memory limits | Review storage growth, account lifecycle, gas accounting and contract host calls. Do not assume all rent/garbage-collection libraries are wired into ledger execution. |
| WASM privilege escalation | Wasmtime sandbox and host adapter | Audit imports, deterministic behavior, memory access, dependency advisories and operator isolation. No blanket claim that every host call is safe. |
| Epoch / restart divergence | Epoch rewards and staking changes use a storage batch; finalized branches also commit atomically | Epoch processing is triggered by the slot loop outside block execution. Audit deterministic timing and persistence across restarts/partitions; short devnet runs do not cover a full epoch. |

## Libraries and proposed services

The node does not assemble QUIC/Turbine/erasure propagation, remote signing,
agent-run settlement dispatch, or an attested AI marketplace. Their presence
in the workspace does not add protections to the running node.

- **TEE:** `crates/verifiers/tee/src/attestation.rs` checks timestamp and approved
  measurement. Its non-simulation certificate-chain path explicitly rejects
  reports because cryptographic certificate verification is unimplemented.
  Simulation bypasses hardware verification. Neither is hardware attestation
  evidence, and simulations must not authorize real paid compute.
- **Agent authorization:** schema, SDK, account-abstraction and FROST verification
  libraries bind policy, receipts and payments. Review signature versus structural
  validation, expiry, nonce lifecycle, spend accounting, guardian provisioning and
  replay across runs/chains. A cross-library E2E does not execute node settlement.
- **Escrow:** `agent-run-escrow` maintains in-memory run and accounting maps.
  Authenticating caller addresses, debiting actual balances, persistent state,
  dispatch and atomic settlement remain integration responsibilities.
- **PQ:** ML-DSA and ML-KEM primitives exist. They do not replace consensus BLS or
  establish a post-quantum P2P handshake. Algorithm tags are not implementations
  of every tagged algorithm.
- **Remote signing:** see [proposal](REMOTE_SIGNER.md). Durable anti-equivocation,
  supported signature algorithms and failover ownership require implementation.

## Privacy (LINDDUN)

Public transaction history can link addresses and activity. Peer connections
expose network metadata, and explorer/indexer deployments can widen retention.
Agent evidence URIs and journal commitments can reveal relationships even when
raw traces stay off-chain. Treat evidence retention, access control, deletion
policy and user notice as explicit application requirements. The current node
does not establish private transactions, anonymous routing, confidential assets
or encrypted mempools.

## Validation and residual risk

The maintained behavioral suite is [E2E-only](../testing.md): in-process node
journeys, cross-library agent settlement, and the four-process local devnet.
These exercise concrete flows; they do not constitute consensus proofs,
fuzzing, penetration tests, hardware attestation or external audits. Unit,
property and doctest suites are not current runnable gates.

Prioritize external review of `HybridConsensus` and quorum arithmetic, ledger
state transitions and epoch processing, cryptographic verification and domain
binding, WASM host isolation, and RPC/P2P resource boundaries. Include library
settlement and bridges in scope before exposing those services. TLA+ assets
are specifications requiring reconciliation with the implementation; their
presence is not an executed model-checking result.

Classical Ed25519/BLS quantum exposure, public transaction linkability, proposer
ordering power, host compromise and dependency risk remain open concerns.
No dated phase commitment or production-readiness claim is implied here.
