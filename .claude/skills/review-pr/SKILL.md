---
name: review-pr
description: High-recall, high-precision independent review of an aether PR. Use when asked to review a PR in jadenfix/arrha (e.g. "/review-pr 400"). Reviews must be done by an agent that did NOT author the PR.
---

# aether PR review

You are an independent, non-author reviewer for `jadenfix/arrha`. The argument is a PR number: `$ARGUMENTS`. aether is an L1 blockchain: a consensus split, a nondeterministic state transition, or value non-conservation is not a bug — it is the end of the chain. Review at that stakes level. This rubric teaches you *how* to find bugs on any PR; it is deliberately not a list of past bugs to grep for.

## Ground rules

- **Non-author only.** Check PR author, commit authors, and current-session authorship with `gh pr view <N> -R jadenfix/arrha --json author,commits`. If you authored any commit, reviewed your own branch, or are uncertain, stop and hand the review to another agent.
- Read-only: do not modify the main clone, do not run `cargo` or `scripts/test.sh` in a directory another agent may be building in. CI already builds per-PR; review by reading.
- Precision: every **blocker** carries a concrete traced failure scenario (specific input/state → specific wrong behavior, with `file:line`). If you cannot trace one, it is a nit.
- Recall: read the ENTIRE diff, the referenced issues, and the surrounding code of every touched file at current `main`. Bugs live at the seams the diff doesn't show — across the Rust workspace, the seams dominate.
- Companion rubrics: for docs/scope PRs, apply `.claude/skills/docs-scope-review/SKILL.md`; for architecture or optimization decisions, apply `.claude/skills/systems-engineering/SKILL.md`; for language, framework, library, runtime, or algorithm choices, apply `.claude/skills/technical-choice/SKILL.md`.

## Procedure

1. `gh pr view <N> -R jadenfix/arrha --json title,body,author,files,mergeStateStatus,statusCheckRollup`
2. `gh pr diff <N> -R jadenfix/arrha` — all of it.
3. `gh issue view <issue> -R jadenfix/arrha` for every referenced issue; the issue defines the intended scope.
4. **Supersession check:** `git log origin/main --oneline -30` plus targeted `git log -p` on touched files → REJECT (superseded) if main already contains an equivalent fix.
5. **Freshness check:** after any wait, force-push, PR body edit, or CI rerun, re-read PR state, head SHA, base SHA, check rollup, and linked issue state.
6. **Overlap check:** `gh pr list -R jadenfix/arrha --state open` — flag open PRs touching the same paths and whether merge order matters.
7. Hunt for bugs using the method below.
8. Return the review body in the fixed format. Post it to GitHub only when the user explicitly asks for a GitHub review action.

## How to find bugs (do this — don't just tick boxes)

- **Trace the assembled path.** Follow node constructors and callers from transaction admission through proposal execution, certification, finality, and atomic persistence, including conflict, revert, timeout, and equivocation branches. Libraries outside that path require separate integration evidence; see `docs/architecture.md`.
- **Review from three seats.** aether serves a **validator** (safety under faults and partitions; can this change make two honest nodes commit different blocks or one honest node get slashed?), an **adversarial peer** (every gossip message, block, vote, shred, and RPC call is attacker-chosen bytes), and a **contract developer / token holder** (deterministic execution, conserved value, honest fees). For the code in the diff, ask how it hurts each of the three.
- **Enumerate failure modes** for every new input, call, or state transition: empty · malformed · oversized · slow/hung · repeated/retried · concurrent · out-of-order · partial failure · adversarial/untrusted — plus blockchain-specific: equivocating · replayed-across-forks · fee-griefing · censorship-inducing.
- **Follow the seams the diff hides:** callers of changed signatures, callees now leaned on, invariants elsewhere that assumed the old behavior.
- **Reverted-fix check:** a behavioral bugfix should have a retained E2E scenario that exercises the failure and fails when the fix is reverted. Unrelated scenarios may still pass. Documentation-only changes need source/command/link checks, not a new test harness.
- **Adversarially verify** each candidate blocker: try to refute it against the code. Survives → blocker. No concrete trace → nit.
- **Preserve durable lessons** under `Durable guidance`; a follow-up author lands accepted guidance in this file from a separate PR.

## What to look for (general bug classes)

Correctness & honesty of the contract:
- [ ] Return values and RPC responses tell the caller the truth — a failed or partially-applied operation is never reported as success; finality status is never overstated (executed ≠ finalized).
- [ ] Docs and declared schemas (RPC, SDKs, explorer) match runtime behavior in the same PR.

Resource, lifecycle & availability:
- [ ] Every network round-trip has a timeout and recovery path; cleanup runs on all exit paths.
- [ ] Locks are narrow and never held across `.await`; consensus-critical tasks must not starve behind RPC or gossip load.

Tests:
- [ ] Retained E2E journeys exercise the changed failure mode and relevant boundaries. The maintainer requires E2E-only tests; use `./cli-test`, with `--in-process-only` for the library/network journeys. Static checks, benchmarks, and fuzzing are separate tools; do not restore unit/property/doctest or JS/Python unit suites.

Fit & simplicity:
- [ ] The change does exactly what its issue needs — no speculative abstraction, dead branch, or unused knob; crate-layer direction respected across the workspace.
- [ ] Architecture and technology choices are switch-worthy: the PR names the baseline, target improvement, rejected alternatives, and smallest proving slice. If not, require the systems-engineering rubric before approving.
- [ ] Documentation changes reduce ambiguity across the public surfaces they touch; moved docs have correct links and PR scope matches the actual change.

## aether-specific bug classes (check every one the diff touches)

Determinism (absolute — a divergence is a consensus split):
- [ ] Nothing host-dependent in the state-transition path: no wall-clock reads, OS randomness, floating point, `HashMap`/`HashSet` iteration order, pointer/allocation-order dependence, or locale/platform-varying behavior. Time and randomness enter only as consensus-provided inputs (block time, VRF output).
- [ ] If the separate parallel scheduler is changed or integrated, scheduling commutes: any R/W-set-legal execution order produces the state a serial execution would. Widened/narrowed R/W set declarations are conservative — an under-declared write set is a determinism blocker.
- [ ] Wasmtime/runtime config changes (feature flags, fuel metering, NaN canonicalization) are evaluated for cross-node reproducibility, and gas/fuel charging is itself deterministic.

Consensus safety (safety > liveness):
- [ ] Changes to vote, lock, quorum, view-change, or VRF-leader logic carry an explicit safety argument in the PR; two honest nodes must never finalize conflicting blocks under ≤ f faults.
- [ ] Equivocation and slashing conditions cannot be triggered against honest nodes by network conditions (delays, reordering, replay); evidence handling validates before punishing.
- [ ] BLS aggregation/verification changes preserve exactly-the-signers semantics — no rogue-key or duplicate-signer acceptance.

Value conservation & ledger rules (eUTxO++):
- [ ] No path creates or destroys value outside protocol rules: sum(inputs) = sum(outputs) + fees (+ protocol mint/burn only where specified); double-spend prevention holds across forks and mempool re-admission.
- [ ] All amount/fee arithmetic is checked (overflow, underflow, rounding direction is specified and consistent); dual-token accounting cannot cross-contaminate.
- [ ] Sparse Merkle state root reflects exactly the committed writes; proofs are verified, never trusted from a peer.

Adversarial P2P and separate DA libraries (attacker-chosen bytes):
- [ ] Every gossip/turbine/RPC input is size-bounded and validated BEFORE allocation or expensive work (signature checks before decode-heavy paths where possible); malformed input can never panic a node — `unwrap`/`expect`/indexing on network-derived data in node crates is a blocker.
- [ ] Per-peer rate/score limits bound what one peer can make a node do (CPU, memory, disk, response amplification); erasure-coding reconstruction handles adversarial shred combinations.
- [ ] Fork-choice and sync cannot be wedged by withheld or contradictory data — a peer that lies about availability degrades that peer, not the node.

TEE/VCR verifier libraries (integration must be established separately):
- [ ] Attestations are verified against expected measurements, never self-reported; a failed or absent attestation fails closed. Verification results entering consensus are deterministic facts (signed artifacts), not re-computed nondeterministically per node.

## Verdict and posting

Default to report-only output unless the user explicitly asked you to post a GitHub review. For a GitHub review, post exactly one review:

```
gh pr review <N> -R jadenfix/arrha --comment --body "<body>"
```

Body format — first line is the verdict, nothing above it:

```
VERDICT: APPROVE | REQUEST-CHANGES | REJECT (superseded | wrong-approach)

<one-paragraph summary: what the PR does, whether it fixes the traced failure>

Blockers:
- <file:line — traced failure scenario>   (or "none")

Nits:
- <file:line — suggestion>                (or "none")

Durable guidance: <candidate reusable invariant for follow-up docs, or "none">

Overlap: <open PRs touching same paths + merge-order note, or "none">

— independent review agent (non-author)
```

APPROVE only with zero blockers. REQUEST-CHANGES when fixable blockers exist. REJECT when superseded or the approach is unsound for consensus. Do not merge — merging is the coordinator's job after CI + mergeability recheck.

## Deep mode (optional)

If asked for a "deep" review, fan out three parallel non-author subagents with distinct lenses — (a) determinism/consensus safety, (b) adversarial-input robustness, (c) economics/value conservation — then adversarially verify each candidate blocker yourself before posting.
