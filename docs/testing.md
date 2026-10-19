# End-to-end validation

Run validation from the repository root. The maintained automated test suite
contains complete journeys only. Rust builds, Clippy, formatting, web builds,
benchmarks, and fuzz tools are separate checks.

## Entry points

```bash
./cli-test                       # in-process journeys, then four-process devnet
./cli-test --in-process-only     # no running node or Docker required
./scripts/test.sh                # delegates arguments to ./cli-test
make test                       # same default as ./cli-test
```

The default runner builds release node/CLI binaries for the process journey.
It needs Rust/Cargo, native C/C++ build dependencies, `curl`, Python 3, and free
RPC/P2P/metrics ports. See [operations](ops/RUNBOOKS.md) for state reset and port
ownership. Run `./cli-test --help` for the accepted flags.

| Suite | What it exercises | Boundary |
| --- | --- | --- |
| [Multi-validator](../crates/node/tests/e2e_multi_validator.rs) | Block/vote/transaction handling, convergence, faults, recovery | In-process nodes; in-memory transport. |
| [Byzantine network](../crates/node/tests/e2e_byzantine_network.rs) | Conflicting votes, slashing, honest-node progress | Selected in-process scenarios. |
| [Adversarial](../crates/node/tests/e2e_adversarial.rs) | Forged messages and invalid transitions through node/consensus/ledger paths | Selected in-process scenarios. |
| [Agent settlement](../crates/sdk/rust/tests/agent_settlement_e2e.rs) | Signing, authorization, escrow, receipts, settlement across libraries | Library journey; some validator fixtures accept signatures. No deployed chain/provider claim. |
| [Process devnet](../scripts/e2e-devnet.sh) | Four validators, DNS and libp2p, CLI/RPC transfers, WASM deployment/calls, finality, crash/rejoin | Real local networking on one machine; defaults `DEVNET_DNS_HOST=localhost` to exercise OS DNS lookup. |

Scenario counts change with the retained journeys and are intentionally not
used as a completion signal. Record a passing execution against an exact
revision. The process journey checks parent QC signer counts and compares the
finalized hash on validators 1 and 4 at a common slot after rejoin.

Library unit and doctest harnesses are disabled. Unit/property sources, old phase
acceptance wrappers, and JS/Python unit-test runners were removed by maintainer
request. Public API examples remain documentation. `cargo test` finding zero
library tests is not an E2E result; use the explicit runner above. Behavioral
changes should extend a meaningful retained journey, with realistic outcomes
and failure conditions, rather than restore removed test categories.

## Static checks and CI

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace --all-features
npm ci
npm run build:web
```

`./scripts/lint.sh` runs formatting, Clippy, and workspace checking. Node 20 is
the web CI version. The Rust workspace declares MSRV 1.86; the current CI uses
stable Rust and does not establish compatibility with that minimum version.

[CI configuration](../.github/workflows/ci.yml) runs:

- formatting/Clippy and a dependency audit that fails on unexcepted vulnerable,
  unsound, or yanked dependencies; unmaintained advisories remain visible;
- the retained in-process journeys and the four-process devnet in separate jobs;
- x86_64 and aarch64 Linux release builds;
- a Docker image build and real Compose RPC transfer journey;
- npm workspace builds for SDK, UI, explorer, and wallet.

These are configured checks, not a claim that any particular hosted run passed.
The Compose journey exercises sibling validators through RPC, while the
four-process devnet journey additionally checks client flows and indexer
persistence/restart. Docker is required for the Compose journey; a source or
image build alone does not establish that container journey passed.

## Reporting evidence

Record the revision, commands, environment, and actual result. Distinguish an
in-process pass, local process-network pass, static build, hosted CI pass, and
remote/operator journey. A local pass does not prove WAN liveness, complete
Byzantine safety, hardware attestation, public deployment, or end-to-end AI
execution/settlement. Benchmark/fuzz outcomes need their own inputs and scope.
The optional crew's peer/domain/crypto/final review ownership is documented in
[AGENT_COMMS.md](../AGENT_COMMS.md).
