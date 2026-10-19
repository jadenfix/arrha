---
name: technical-choice
description: Language, framework, library, runtime, data-structure, and algorithm selection for jadenfix/arrha. Use when choosing what technology or algorithm to use, replacing an existing approach, optimizing for performance or adoption, or deciding whether a new dependency/framework is justified.
---

# aether technical choice

Use this skill when the question is "what should we use?" or "is this approach good enough to switch?" The answer must be grounded in constraints and measured improvement, not taste.

## Non-negotiable standard

Only recommend a change when it is materially better than the baseline for this repository. "Materially better" means it improves a named constraint enough to justify migration, maintenance, and adoption cost.

If no option clears that bar, recommend keeping the current approach and name the missing evidence.

## Decision workflow

1. Define the workload shape: hot path vs offline, adversarial vs trusted input, latency vs throughput, read/write ratio, state size, concurrency, and deployment target.
2. Identify hard constraints: determinism, value safety, resource bounds, security boundary, portability, SDK ergonomics, ecosystem compatibility, and operator burden.
3. Establish the baseline: current language/framework/algorithm, known bottleneck, failure mode, and maintenance cost.
4. Compare options against the baseline, including "do nothing" and "small local improvement."
5. Reject options that improve a benchmark while weakening a stronger invariant.
6. Choose the smallest reversible proving slice before broad migration.

## Default choices

- Use Rust for consensus, ledger, mempool, networking, cryptography, deterministic execution, adversarial input handling, and long-running services.
- Use TypeScript for browser surfaces, wallet/explorer UX, Node-facing SDKs, and developer-facing ecosystem glue.
- Use Python for offline analysis, fixtures, release tooling, and one-shot operator scripts where correctness is externally checked and the code is not on a trust boundary.
- Use shell only for thin orchestration. Do not encode complex parsing, security policy, or portability-sensitive behavior in shell.
- Use SQL or embedded storage only when query patterns and durability semantics are explicit; do not add a database to hide unclear state ownership.
- Prefer standard-library or well-maintained crates over bespoke algorithms unless the invariant is repo-specific and tested at boundaries.

## Algorithm selection checks

- State the complexity in worst-case terms, not only average case.
- Show the memory bound and live-state quota.
- Prove deterministic iteration order where outputs affect consensus, signatures, hashes, fees, or public API ordering.
- Prefer streaming, bounded, or incremental algorithms for untrusted remote data.
- Avoid probabilistic structures in consensus-visible paths unless false-positive/false-negative behavior is explicitly harmless.
- Exercise below, at, and above each changed cap within retained E2E journeys; use static checks and separate benchmark/fuzz tools where appropriate.

## Framework and dependency checks

- New dependencies must reduce total complexity or risk, not just line count.
- Frameworks must make lifecycle, cancellation, error handling, and observability clearer.
- Public APIs must remain stable enough for SDKs and generated clients.
- Security posture must be at least as strong as the current approach: auth, replay scope, secret handling, supply-chain risk, and downgrade behavior.

## Output format

```text
Choice: <selected option or no-change>
Problem: <one sentence>
Baseline: <current approach and bottleneck>
Hard constraints: <invariants that dominate>
Options: <brief comparison including do-nothing>
Why this wins: <specific material improvement>
Rejected because: <why alternatives fail>
Proving slice: <small implementation + evidence>
Docs/contracts to update: <README/API/SDK/schema/runbook/etc.>
```
