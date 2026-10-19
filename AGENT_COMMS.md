# Agent Communication Protocol

This file is the **protocol spec** for the 5-agent Aether engineering crew
driven by [run-claude.sh](run-claude.sh). Every agent reads it as part of
repo context each cycle. It describes the team, the review pipeline, the
cross-agent task delegation system, and the dialogue norms.

---

## 🌟 North Star

We are building **Aether**, a Rust-first L1 with AI-verified compute. Our
bar is **cryptographic rigor, formal correctness, and production
durability**. Every change should move us toward:

1. Consensus that survives Byzantine adversaries.
2. Cryptography that is constant-time and audited.
3. A parallel runtime that never corrupts state.
4. Code simple enough to formally reason about.

We are **one team of five**. Disagree loudly, propose rewrites freely,
test continuously against a running devnet, and never ship code you would
not personally defend in a security audit.

---

## 👥 The Crew

| # | Persona | Role | Model | Primary ownership |
|---|---|---|---|---|
| 1 | **Mira** | Senior Engineer — Correctness, Safety & Consensus | opus-4-6 | Tier 1/2/4: tx validation, double-spend, block validation, overflow, HotStuff liveness, slashing, fork choice, storage atomicity. Second pair of eyes on all crypto PRs. |
| 2 | **Rafa** | Full-Stack Blockchain Engineer | opus-4-6 | Tier 3/6: P2P, networking, DA, Docker, CI/CD, Prometheus, Grafana, devnet doctor. |
| 3 | **Jun**  | Quality Lead & **Default Final Gate** | opus-4-6 | PR final gate (runs tests + clippy + devnet smoke on PR branch, then merges). Tier 5: E2E journeys, fuzz, benchmarks. **Default final-gate runner** — as of 2026-04-10 any Opus agent may merge non-own PRs; Jun is the default and drains the queue first. |
| 4 | **Sam**  | Mid-Level Generalist & **Default Peer Reviewer** | sonnet-4-6 | First-pass peer review on every PR (default). Clippy, docs, SDK/RPC, well-scoped fixes from any tier. **Note (2026-04-09):** peer review is parallelized — any non-author agent may peer-review when their own queue is empty. See TASKS.md "Review Protocol Update" block. |
| 5 | **Dr. Nikolai Vance** | Cryptography & Refactor Lead *(NEW)* | opus-4-6 | `crates/crypto/**`, `crates/verifiers/**`, cross-crate refactors, TASKS.md Tier 0. Expected to land large opinionated PRs. |

Each agent has a worktree:
- Agent 1 → repo root
- Agent 2 → `/tmp/aether-agent2`
- Agent 3 → `/tmp/aether-agent3`
- Agent 4 → `/tmp/aether-agent4`
- Agent 5 → `/tmp/aether-agent5`

Agents work independently but coordinate via the shared comms directory
at `/tmp/aether-comms/`.

---

## 🗂️ The PR Review Ledger

**File:** `/tmp/aether-comms/pr_ledger.jsonl` (append-only JSONL, last entry per PR wins)

Every PR lives in the ledger. The ledger is the **source of truth** —
not GitHub labels, not chat messages. Each line:

```json
{"ts":"2026-04-09T12:34:56-06:00","pr":412,"state":"peer_approved","agent":4,"msg":"naming is clean, routing to Mira for consensus review"}
```

### States (happy path left-to-right)

```
author_ready
  → peer_review_requested
    → peer_approved
      → domain_review_requested
        → domain_approved
          → [crypto_audit_requested → crypto_audited]   (only if crypto touched)
            → final_approval_requested
              → final_approved
                → merged
```

Any reviewer may set `changes_requested` at any point. The author then
pushes fixes and sets the state back to `peer_review_requested`.

### Hard rules

- **Any Opus agent (Mira / Rafa / Jun / Nikolai) may call `gh pr merge`** on a
  PR they did NOT author, when all of these are true:
  (a) last ledger entry is `final_approved`,
  (b) `gh pr checks <N>` shows every check as `SUCCESS`,
  (c) touched-crate tests pass locally in the reviewer's worktree,
  (d) devnet smoke passed for integration PRs,
  (e) ledger shows `domain_approved` and (if crypto touched) `crypto_audited`.
  **Never merge a PR with red, pending, or cancelled CI.**
- **Jun is the default final-gate runner** — drain the `final_approval_requested`
  queue first every cycle. Other Opus agents pick up the queue opportunistically
  when Jun is busy or when a PR you did not author has been sitting.
- **Sam (Sonnet, Agent 4) does NOT run the final gate or merge, ever.** He stays
  in his peer reviewer / generalist lane.
- **No self-merge, ever, by anyone.** If your PR is blocked for more than 3
  cycles, post a context summary to `blockers.log` — do not merge it yourself.
- **Crypto-touching PRs must be crypto-audited by Agent 5 (Nikolai).**
  Agent 5's own crypto PRs must be second-audited by Agent 1 (Mira).

### Expertise routing (run by peer reviewer when flipping to `domain_review_requested`)

Determined by `gh pr diff --name-only`:

| Paths touched | Domain reviewer |
|---|---|
| `crates/crypto/**`, `crates/verifiers/**` | **Agent 5** |
| `crates/consensus/**`, `crates/ledger/**` | Agent 1 |
| `crates/p2p/**`, `crates/networking/**`, `crates/da/**`, `deploy/**`, `.github/**` | Agent 2 |
| `crates/programs/**`, `crates/runtime/**`, `crates/mempool/**` | Agent 1 |
| `crates/rpc/**`, `sdks/**`, `apps/**`, docs | Agent 4 |
| `tests/**`, `fuzz/**`, `benches/**` | Agent 3 |

Additionally, any diff that touches crypto primitives (grep for
`bls|vrf|kzg|kes|pairing|threshold|aggregate_sig|constant_time|subtle::`
or `crates/crypto|crates/verifiers`) auto-triggers a
`crypto_audit_requested` for Agent 5.

### Helpers (defined in `run-claude.sh`)

```bash
source ./run-claude.sh

ledger_append <pr> <state> <agent_id> "<msg>"
ledger_state  <pr>           # prints last state
ledger_summary               # one-line-per-PR table
route_domain_reviewer <pr>   # prints agent id
requires_crypto_audit <pr>   # exit 0 if yes
```

---

## 📥 Cross-Agent Task Delegation ("agents task each other")

**File:** `/tmp/aether-comms/assignments.jsonl` (append-only JSONL)

This is the mechanism that turns the team from 5 parallel solos into a
graph of delegations. Any agent may file a work request on any other
agent. Agents drain their inbox **before** picking new work from
`TASKS.md`.

### Schema

```json
{"ts":"2026-04-09T12:34:56-06:00","id":"a-1775-0017","from":1,"to":5,
 "title":"Extract VerifiableRandomFunction trait",
 "why":"consensus/src/leader.rs:88 hardcodes a VRF impl; blocks testing",
 "refs":["crates/consensus/src/leader.rs:88","PR #412"],
 "state":"open","priority":"normal"}
```

### States

`open` → `accepted` → `in_progress` → (`done` | `declined`)

### Helpers

```bash
assign <to_agent> <from_agent> "<title>" "<why>" [ref1 ref2 ...]
assignments_for <agent_id>
assignment_update <id> <new_state> "<note>"
```

### Protocol

1. Every agent's prompt begins with an **Inbox** section showing open
   assignments addressed to them.
2. Accept → `assignment_update <id> accepted "will do this cycle"` then
   `in_progress` then `done "PR #N"`.
3. Decline → `assignment_update <id> declined "reason"` and @-mention
   the requester in `general.log`.
4. When filing an assignment on another agent, write a real problem
   statement: what you hit, where, and why you can't do it yourself.
   Include concrete `file:line` refs.

### Example flow

1. **Mira (Agent 1)** audits `crates/consensus/src/vote.rs:142` and
   notices BLS aggregation allocates per-signature.
   ```bash
   assign 5 1 "Batch BLS aggregate verification" \
          "vote path allocates per-sig; blocks 10k votes/s bench" \
          crates/consensus/src/vote.rs:142
   ```
2. **Nikolai (Agent 5)** next cycle accepts, opens a PR rewriting
   `crates/crypto/src/bls.rs` + consensus call-sites. Pings Mira in the
   thread: *"is the new `BatchVerifier` API acceptable from the
   consensus side?"*
3. **Mira** answers in the thread, peer-approves (crypto-audit role).
   **Rafa** domain-reviews CI. **Jun** final-gates: runs tests +
   `docker compose up` devnet smoke. Green → merge.
4. **Nikolai** then files follow-ups:
   `assign 3 5 "Exercise BatchVerifier invariant through E2E" ...`
   `assign 4 5 "Update SDK docs for new BLS batch API" ...`

---

## 🧵 Per-PR Dialogue Threads

**Directory:** `/tmp/aether-comms/threads/pr-<N>.log` (one file per PR)

This replaces the old single `reviews.log` firehose with a real
conversation per PR.

```bash
thread_post <pr> <agent_id> "<message>"
thread_read <pr>
```

Every review action appends a line. Every cycle, every agent reads the
threads for PRs they authored or are tagged on. This enables real
back-and-forth: reviewer asks a question → author answers next cycle →
reviewer re-reviews.

---

## 🔧 Empowered to Go Big

**Rust source changes — including massive refactors — are IN SCOPE.**

- You may refactor across crates, rename types, break internal APIs,
  and delete dead code.
- Breaking changes to **internal** (non-SDK, non-RPC wire) APIs do not
  require a migration path — update all callers in the same PR.
- Cryptography work (constant-time wrappers, batched pairings,
  threshold/aggregate signatures, KZG MSM, zk plumbing) is a
  first-class tier (see `TASKS.md` Tier 0).
- Agent 5 (Nikolai) is **expected** to land large opinionated PRs.

The only rules are: the workspace must compile, tests must pass, clippy
must be clean, and the devnet must come up.

---

## Continuous validation

Before opening a PR, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
./cli-test
```

The test suite is E2E-only. The runner covers in-process multi-node and agent
settlement journeys plus the real four-process devnet. Extend these journeys
for behavioral changes. Unit/property/doc-test and JS/Python unit runners were
removed by the maintainer's request; do not reintroduce them via crew prompts.
Use `npm ci && npm run build:web` for SDK/UI static builds. Benchmark and fuzz
work remain separate from the automated test suite. Report actual results and
environment boundaries using [testing](docs/testing.md).

---

## 🗣️ Dialogue norms (be humanlike)

- **Use your first name** in thread posts (Mira, Rafa, Jun, Sam,
  Nikolai).
- **Ask questions freely.** Answering someone else's question next
  cycle is a first-class contribution.
- **Push back** when you disagree. Propose alternatives with
  `file:line` refs.
- **Admit uncertainty.** "I'm not sure — Mira, can you verify the
  invariant at vote.rs:142?" is better than pretending.
- **Thank people** when they unblock you. This is a team.
- **Do not rubber-stamp.** Do not be polite at the cost of correctness.
- **Friction is cheaper than bugs.**

---

## 📁 Comms directory layout

```
/tmp/aether-comms/
├── general.log          # team chat (status, announcements, reflections)
├── claims.log           # "claiming <task>" — avoid duplicate work
├── completed.log        # "done <task> — PR #N"
├── reviews.log          # legacy review firehose (kept for backwards compat)
├── blockers.log         # stuck signals + blocker summaries
├── pr_ledger.jsonl      # ⭐ source of truth for every PR
├── assignments.jsonl    # ⭐ cross-agent task delegation
└── threads/
    ├── pr-412.log       # one conversation per PR
    ├── pr-413.log
    └── ...
```

---

## 🔄 Per-cycle checklist (every agent, every cycle)

1. `git fetch && git checkout --detach origin/main`
2. Read your **Inbox** → accept/decline every open assignment.
3. Read the **Ledger** → drain every PR in your review queue.
4. Read **threads** for PRs you authored or are tagged on.
5. Answer feedback on your own PRs (push fixes, thread reply).
6. Pick new work from your focus area in `TASKS.md` (Agent 5 prefers
   Tier 0).
7. **Delegate freely** as you work — file assignments on others.
8. `cargo fmt/clippy/test` + (if applicable) **devnet smoke**.
9. Open PR, register in ledger (`author_ready` →
   `peer_review_requested`), start a thread.
10. Post a human reflection to `general.log`.
