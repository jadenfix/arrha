# Aether Security Audit Scope

Reviewed **2026-10-18**, implementation baseline `b5460e8` (PR #413).
This document scopes a security review of the repository. The goal is to focus audit effort on the code paths and interfaces that carry the highest protocol and operational risk while also accounting for the project’s current delivery model.

## Audit Goals

- assess consensus, runtime, ledger, and state-transition correctness;
- evaluate cryptographic and verifier boundaries;
- review external interfaces such as RPC, CLI, SDK-adjacent surfaces, and deployment assets; and
- identify mismatches between repository content, CI coverage, and operational assumptions.

## Priority Tiers

### Tier 1: Protocol-Critical

Highest priority directories and crates:

- `crates/consensus`
- `crates/node`
- `crates/runtime`
- `crates/ledger`
- `crates/state/*`
- `crates/crypto/*`
- `crates/rpc/json-rpc`

Primary concerns:

- consensus safety and liveness failures;
- invalid state transitions or double-spend conditions;
- signature, proof, or slashing-verification bugs;
- runtime determinism and execution sandbox boundaries; and
- RPC exposure or malformed input handling.

### Tier 2: Network, Programs, and Verification

- `crates/mempool`
- `crates/p2p`
- `crates/networking/*`
- `crates/da/*`
- `crates/programs/*`
- `crates/verifiers/*`
- `crates/mev`
- `crates/rollup`

Primary concerns:

- peer-manipulation and message-handling bugs;
- denial-of-service vectors;
- economic or program-level invariant violations; and
- verifier boundary failures.

### Tier 3: AI Mesh, Tooling, and Delivery Surfaces

- `ai-mesh/*`
- `crates/tools/*`
- `sdks/*`
- `apps/*`
- `deploy/*`
- `Dockerfile`
- `Dockerfile.test`
- `docker-compose.test.yml`
- `.github/workflows/ci.yml`

Primary concerns:

- incorrect assumptions in attestation and AI service coordination;
- tooling behavior that could lead operators into unsafe states;
- insecure default deployment configuration; and
- delivery gaps between validated and unvalidated surfaces.

## Trust Boundaries

Key trust boundaries in the current repository include:

- client and operator input into JSON-RPC and CLI surfaces;
- validator-to-validator communication across networking and consensus layers;
- state persistence and recovery boundaries;
- proposed AI mesh and agent-settlement interactions with on-chain logic (not assembled in the running node); and
- deployment/configuration boundaries introduced by Docker, Compose, Helm, Kubernetes, and Terraform assets.

## Evidence Available to Auditors

Distinguish reusable libraries from running-node integration using
[architecture](../architecture.md). Automated behavioral checks are E2E-only;
unit/property/doctest and JS/Python unit suites were removed. Workflow presence
is not an executed audit or production qualification.

The repository already includes supporting material that can be used during audit preparation:

- `.github/workflows/ci.yml`
- `scripts/e2e-devnet.sh`
- `docs/testing.md`
- `scripts/lint.sh`
- `scripts/test.sh`
- `scripts/docker-test.sh`
- `docs/security/THREAT_MODEL.md`
- `docs/security/REMOTE_SIGNER.md`
- `docs/architecture.md`
- `docs/ops/RUNBOOKS.md`

## Current Delivery Constraints to Account For

Auditors should account for the project’s present operating model:

- GitHub Actions defines Rust lint/build, in-process E2E, real four-process devnet E2E, web builds and Docker image builds; workflow presence is distinct from a passing run on the audited commit. It does not publish releases or deploy.
- The web-build job runs `npm ci` and `npm run build:web`. This is build coverage, not browser E2E or Python SDK qualification.
- `cargo audit` is allowed to fail; a passing workflow is not proof of a clean dependency audit.
- Deployment assets exist for multiple environments, but rollout is still operator-driven rather than CI-driven.
- Some deployment and infrastructure assets may therefore require manual review in addition to code review.

## Recommended Audit Order

1. Tier 1 protocol-critical crates and interfaces.
2. Tier 2 networking, program, and verification crates.
3. Tier 3 AI mesh, tooling, and deployment surfaces.

## Readiness Checklist

- [x] CI workflow present for lint, Rust/web builds, Docker image build, in-process E2E and four-validator devnet E2E.
- [x] Threat model and remote-signer design documents present in `docs/security/`.
- [x] Docker-based local and test environments present.
- [ ] Browser/client E2E and target-environment validation completed where required.
- [ ] Environment-specific deployment validation completed for the intended target environments.
- [ ] Final audit package assembled with pinned commits, reproducible build instructions, and operator assumptions.

## Integration-sensitive findings

- Audit `HybridConsensus` as the active consensus implementation; removed
  standalone engines are not alternative runtime modes.
- The node uses TCP/Noise/Yamux, sequential WASM execution and finalized overlays;
  QUIC/DA and the parallel scheduler are separate library review scopes.
- Non-simulation TEE certificate verification currently refuses reports because
  the cryptographic chain check is unimplemented. Do not treat simulation
  acceptance as hardware evidence.
- Agent escrow accounting is a library state machine. Review caller
  authentication, actual balance movement and persistence before node exposure.
- The remote-signer document is a proposal. There is no operational HSM path
  to qualify yet.
- Epoch rewards are written in a slot-driven storage batch outside blocks;
  review their timing and recovery separately from finalized block commits.
