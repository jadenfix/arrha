---
name: systems-engineering
description: Rigorous systems engineering and technology selection for jadenfix/arrha. Use when choosing architecture, language, framework, algorithm, data model, runtime, protocol boundary, optimization strategy, or security/resource-control design, especially when the result must be materially better than the current approach.
---

# aether systems engineering

Use this skill when a change needs a real engineering decision, not just implementation. Aether is a blockchain and agent-infrastructure project; correctness, determinism, security, resource bounds, operability, and adoption all matter.

## Decision standard

Do not optimize for novelty. Optimize for a switch-worthy improvement: a simpler, safer, faster, more reliable, or more adoptable system with a named metric or invariant that improves enough to justify migration cost.

A decision is not ready until it states:

- The user or operator problem.
- The invariant that must never break.
- The current baseline and bottleneck.
- The target improvement and how it will be measured.
- The rejected alternatives and why they are worse for this repo.
- The smallest reversible slice that proves the direction.

## Selection heuristics

- Use Rust for consensus, ledger, networking hot paths, cryptography, deterministic execution, long-running services, and anything that handles adversarial bytes or value.
- Use TypeScript for browser apps, wallet/explorer UX, Node-facing SDKs, and ecosystem glue where developer adoption matters.
- Use Python for offline tooling, analysis, fixtures, and operator scripts only when it is not consensus-critical and performance/security boundaries are explicit.
- Use shell only for thin orchestration around existing tools. Avoid complex parsing or policy logic in shell.
- Avoid adding a framework when a crate, module, or small protocol artifact solves the problem. New frameworks must reduce total complexity, not move it.
- Prefer explicit wire contracts over source coupling between ecosystem repos.
- Prefer deterministic, bounded algorithms over average-case cleverness in consensus, mempool, runtime, and network-facing code.
- Prefer streaming or incremental caps over post-hoc size checks for remote-driven JSON, logs, DOM, screenshots, diffs, and tool results.

## Algorithm and architecture review

For each option, evaluate:

- Correctness proof: what invariant it preserves and where enforcement lives.
- Complexity: time, memory, disk, network, and worst-case adversarial input.
- Determinism: no host clock, randomness, floating point, unordered iteration, or platform-specific behavior in state-transition paths.
- Security boundary: authentication, authorization, origin/host checks, secret handling, replay scope, and downgrade behavior.
- Resource control: live-state quota, in-flight limits, backpressure, cancellation, and cleanup.
- Operability: metrics, logs, migration path, rollback, and failure-mode diagnosis.
- Adoption: API clarity, SDK ergonomics, docs, compatibility, and whether switching is obviously worth it.

## Output format

Use this structure for architecture recommendations:

```text
Decision: <chosen option>
Problem: <one sentence>
Baseline: <current behavior and bottleneck>
Target: <metric or invariant that gets materially better>
Options considered: <brief comparison>
Why this wins: <specific technical reason>
Risks: <failure modes and mitigations>
Smallest proving slice: <minimal implementation and validation evidence>
Docs/contracts to update: <public surfaces>
```

If no option is materially better, say so and recommend not changing the system yet.
