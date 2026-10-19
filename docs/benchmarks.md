# Devnet benchmarks

Historical measurements from **2026-10-18**, at commit `ed0961e`, using
`scripts/bench/run-benchmarks.sh`. These numbers have not been remeasured after
the cleanup merged in PR #413; source/documentation updates do not refresh
performance evidence.
Raw results are in [`bench/results/`](../bench/results/) and the summary is in
[`bench/results/summary.json`](../bench/results/summary.json).

**What this is:** 4 validator processes (`aether-node`) on **one Apple M4 laptop**
(10 cores: 4 performance + 6 efficiency, 16 GB RAM, macOS 26.6.2) talking to each other
over **localhost**. The load generator runs on the same machine. Binaries were built with
`cargo build --release` using thin LTO (`CARGO_PROFILE_RELEASE_LTO=thin`,
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`); rustc 1.99.1.

**What this is not:** a wide-area network, a large validator set, an adversarial setting,
or a test with real users. Loopback latency is close to zero and all four validators
share one CPU with the load generator. Do not compare these numbers with production
networks as if they were like-for-like.

## Parameters at the measured revision

| parameter | value |
|---|---|
| slot length | 500 ms (`ChainConfig::devnet`) |
| leader election | VRF, tau = 0.8: each validator is eligible in a slot with probability 0.2, so about 41% of slots have no leader and about 18% have two |
| commit rule | 2-chain: a block is final when it and its child **in the next slot** both have quorum certificates (>= 2/3 stake = 3 of 4 BLS votes) |
| block limits | 10,000,000 gas and 1,000 txs per block; a transfer declares 21,000 gas (max 476 per block), a counter-contract call 100,000 (max 100 per block) |
| mempool | at most 100 tx/s admitted per sender, so the load generator spreads load over >= rate/20 funded devnet accounts |

## Method

Every scenario starts a fresh devnet (`scripts/devnet.sh clean && start`: new genesis,
empty RocksDB). The load generator (`aether-loadgen run`) signs transactions from the
deterministic devnet accounts, submits them at a fixed offered rate in batches
(`aeth_sendRawTransactions`) pinned per sender to one of the 4 RPC endpoints, and
records each transaction's submission time. After the run it reads finalized blocks
(`aeth_getBlockSummaries`), which include the time the node finalized each block on the
same machine clock, and computes per-transaction **submission to finality** latency.
Each sweep point runs for 40 s (first 10 s excluded from latency percentiles) followed by
up to 45 s for the backlog to drain. "Lost" counts accepted transactions that never
finalized.

## Results

### Simple transfers

| offered tx/s | finalized | lost | finality p50 | p95 | p99 |
|---:|---:|---:|---:|---:|---:|
| 100 | 4,000 / 4,000 | 0 | 1.57 s | 4.63 s | 6.22 s |
| 200 | 8,000 / 8,000 | 0 | 2.13 s | 6.56 s | 7.76 s |
| 300 | 12,000 / 12,000 | 0 | 1.72 s | 3.76 s | 4.29 s |
| 400 | 16,000 / 16,000 | 0 | 2.05 s | 5.90 s | 8.38 s |
| 500 | 20,000 / 20,000 | 0 | 3.17 s | 24.5 s | 34.7 s |

**Observed stable offered rate in these short runs: about 400 transfers/s.** At 500 tx/s blocks are full
(462 tx mean) and a queue builds (p95 24.5 s), so 500 is above capacity.

### WASM contract calls (counter contract, `examples/contracts/counter.wat`)

| offered calls/s | finalized | lost | finality p50 | p95 | p99 |
|---:|---:|---:|---:|---:|---:|
| 25 | 1,000 / 1,000 | 0 | 1.79 s | 4.52 s | 5.32 s |
| 50 | 2,000 / 2,000 | 0 | 2.67 s | 5.83 s | 6.33 s |
| 100 | 4,000 / 4,000 | 0 | 2.62 s | 8.70 s | 12.1 s |
| 150 | 6,000 / 6,000 | 0 | 3.66 s | 30.1 s | 35.6 s |

**Observed stable offered rate in these short runs: about 100 contract calls/s.**
The declared gas budget caps calls per block; this sweep does not isolate WASM
execution time or prove the only system bottleneck.

### Block time and finality on an idle network (60 s, no load)

* Slots are 500 ms; 61 blocks were finalized over 124 slots (0.49 per slot), i.e. one
  finalized block every **1.03 s** on average.
* Time from a block first being seen to final: p50 **1.5 s**, p95 5.0 s, p99 5.5 s.
  Most of that is waiting for a next-slot child to be certified: with VRF leader
  election, a block often has no leader in the following slot.

### One validator down (f = 1) and rejoin

100 transfers/s for 90 s; validator 3 killed at t = 25.0 s and restarted at t = 55.7 s.

* Before the kill: 24 blocks finalized in 25 s. **While down: 29 blocks finalized in
  30.6 s** (3 of 4 validators are exactly a quorum). The longest gap between
  finalized blocks while down was 7.5 s.
* After restart the validator requested the blocks it missed and finalized a block again
  **1.8 s** after it was restarted.
* All 9,000 transfers finalized (p50 2.2 s, p95 5.8 s, p99 7.2 s).

### BLS signature verification (blst, 1,024 signatures, median of 3 runs)

| method | signatures/s |
|---|---:|
| one at a time | 2,111 |
| batch (random linear combination, distinct messages) | 12,324 (5.8x) |
| aggregate, same message (how quorum certificates are checked) | 8,415 incl. aggregation |

## Limitations recorded for the measured revision

* Epoch-boundary reward distribution (every 43,200 slots, about 6 h on devnet) still
  writes balances outside of blocks; none of these runs reached an epoch boundary.
* Epoch randomness uses the deterministic per-epoch fallback; mixing in VRF outputs was
  removed from the hot path because validators applied it at different times.
* The 2-chain commit rule and its implementation have not been audited or model-checked
  against `specs/tla/HotStuffVRF.tla`.

## Reproduce

```bash
CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo build --release -p aether-node -p aether-cli -p aether-loadgen --bins
OUT_DIR="$PWD/work/bench-results" ./scripts/bench/run-benchmarks.sh
python3 scripts/bench/summarize.py work/bench-results
```

The driver cleans its `DEVNET_DIR` for each scenario, uses RPC ports 8545–8548,
and stops the benchmark devnet on exit. Stop other users of those ports first.
Keep the checked-in `bench/results/` as historical evidence; the command above
writes fresh output separately. Capture the new commit, compiler, hardware and
settings alongside any replacement measurements. No benchmark was executed
for this documentation revision.
