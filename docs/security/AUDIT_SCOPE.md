# Aether Security Audit Scope

This document scopes a security review of the repository as it exists today. The goal is to focus audit effort on the code paths and interfaces that carry the highest protocol and operational risk while also accounting for the project’s current delivery model.

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
- AI mesh service interaction with on-chain verification logic; and
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

- GitHub Actions validates the Rust and Docker test paths, but it does not currently publish release artifacts.
- TypeScript and frontend workspaces are present in the monorepo but are not part of the current GitHub Actions workflow.
- Deployment assets exist for multiple environments, but rollout is still operator-driven rather than CI-driven.
- Some deployment and infrastructure assets may therefore require manual review in addition to code review.

## Recommended Audit Order

1. Tier 1 protocol-critical crates and interfaces.
2. Tier 2 networking, program, and verification crates.
3. Tier 3 AI mesh, tooling, and deployment surfaces.

## Readiness Checklist

- [x] CI workflow present for lint, test, build, Docker, integration, and acceptance-script execution.
- [x] Threat model and remote-signer design documents present in `docs/security/`.
- [x] Docker-based local and test environments present.
- [ ] Delivery automation expanded to cover non-Rust workspaces where required.
- [ ] Environment-specific deployment validation completed for the intended target environments.
- [ ] Final audit package assembled with pinned commits, reproducible build instructions, and operator assumptions.
