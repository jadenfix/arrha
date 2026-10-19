# End-to-end validation

The automated test suite contains complete journeys only. Static builds,
Clippy, formatting, benchmarks, and fuzz tools are separate checks.

## Entry points

```bash
./cli-test
./cli-test --in-process-only
./scripts/test.sh            # same default as ./cli-test
make test                   # same default as ./cli-test
```

| Suite | What it exercises | Boundary |
| --- | --- | --- |
| `crates/node/tests/e2e_multi_validator.rs` | Multi-node block/vote/transaction handling, convergence, faults, and recovery | In-process nodes; in-memory transport. |
| `crates/node/tests/e2e_byzantine_network.rs` | Conflicting votes, slashing, and honest-node progress | In-process network. |
| `crates/node/tests/e2e_adversarial.rs` | Forged messages and invalid transitions through node/consensus/ledger paths | In-process network; selected adversarial scenarios. |
| `crates/sdk/rust/tests/agent_settlement_e2e.rs` | Signing, authorization, escrow, receipts, and settlement across libraries | Library journey; some validator fixtures accept signatures. No deployed chain/provider integration claim. |
| `scripts/e2e-devnet.sh` | Four validator processes, libp2p, CLI/RPC transfers, WASM deploy/call, finality, crash/rejoin | Real local networking on one machine. |

Library unit and doctest harnesses are disabled. Unit/property sources, old
phase acceptance wrappers, and JS/Python unit-test runners have been removed.
Public API examples remain documentation. Do not treat `cargo test` finding zero
library tests as an E2E result; use the explicit runner above.

CI runs the in-process suites and the process devnet in separate jobs. It also
checks Rust formatting/Clippy, Linux release builds, Docker image buildability,
and TypeScript workspace builds. The dependency audit currently allows failure;
it is not a blocking security gate. See `.github/workflows/ci.yml`.

A passing local journey does not prove WAN liveness, complete Byzantine safety,
hardware attestation, public deployment, or end-to-end AI execution/settlement.
