# Optional autonomous crew protocol

[run-claude.sh](run-claude.sh) contains the optional five-agent launcher, prompts,
and shell helpers. This protocol preserves its ownership and review boundaries.
Read [CLAUDE.md](CLAUDE.md), [TASKS.md](TASKS.md), and
[testing](docs/testing.md) for current implementation and validation guidance.
The crew is separate from one-off subagents requested in a chat. This file does
not itself authorize starting paid agents, contacting others, or merging work.

## Ownership and worktrees

| ID | Persona | Role and primary ownership |
| --- | --- | --- |
| 1 | Mira | Consensus, transaction/block correctness, runtime/state safety; second crypto audit for Agent 5's work. |
| 2 | Rafa | P2P/networking/DA, operator assets, CI, observability, devnet health. |
| 3 | Jun | E2E journeys, separate fuzz/benchmark work; default final-gate reviewer. |
| 4 | Sam | SDK/RPC, apps, docs, scoped fixes; default peer reviewer. Never final-gates or merges. |
| 5 | Nikolai | Cryptography, verifier boundaries, cross-crate abstractions; primary crypto auditor. |

The launcher configures Mira/Rafa/Jun/Nikolai with `claude-opus-4-6` and Sam with
`claude-sonnet-4-6` persona model settings. Inspect the actual launcher/runtime
configuration before inferring which executable/model a session uses.

Agent 1 uses the repository root; agents 2–5 use `/tmp/aether-agent2` through
`/tmp/aether-agent5`. Inventory branch, HEAD, dirty files, and active processes
before work. Preserve another contributor's changes. Fetch/check out a base or
PR only in your own clean worktree; never reset a shared dirty checkout.

The queue uses domain ownership, not historical phase/tier numbers. Sam reviews
first by default; any non-author agent may peer-review when their own queues are
empty. Jun drains final review by default; Mira, Rafa, or Nikolai may pick it up.

## PR review ledger

`COMMS_DIR` defaults to `/tmp/aether-comms`; `LEDGER`, `ASSIGNMENTS`, and
`THREADS_DIR` can override individual paths. The PR ledger is append-only JSONL;
the last entry for a PR gives its current state. Read its earlier entries to
verify the approval trail. It coordinates crew review; GitHub checks/reviews and
the reviewed commit remain independent evidence.

```json
{"ts":"2026-10-18T12:00:00-07:00","pr":123,"state":"peer_approved","agent":4,"msg":"reviewed SDK documentation against current source"}
```

```text
author_ready → peer_review_requested → peer_approved
  → domain_review_requested → domain_approved
  → [crypto_audit_requested → crypto_audited]
  → final_approval_requested → final_approved → merged
```

Use `changes_requested` for feedback at any stage. After fixes, the author
returns the PR to `peer_review_requested`. Crypto and domain review can proceed
in parallel, but both approvals must be in the trail before the final gate.

| Changed paths/domain | Reviewer |
| --- | --- |
| `crates/crypto/**`, `crates/verifiers/**` | Agent 5; Agent 1 second-audits Agent 5's own crypto work |
| `crates/consensus/**`, `crates/ledger/**`, `crates/programs/**`, `crates/runtime/**`, `crates/mempool/**` | Agent 1 |
| `crates/p2p/**`, `crates/networking/**`, `crates/da/**`, `deploy/**`, `.github/**` | Agent 2 |
| RPC, SDKs (`crates/sdk/**`, `sdks/**`), apps, docs | Agent 4 |
| E2E test targets, fuzz, benchmarks | Agent 3 |

These are ownership rules. `route_domain_reviewer` is only a routing hint: its
pattern list is narrower than this table, selects an early path match, and has
an Agent 1 fallback. Read the full diff and request every relevant domain's
review. `requires_crypto_audit` detects crypto paths/keywords; it is not proof
that a diff without a match has no cryptographic impact.

## Final review and validation

A crew merge requires all of the following on the current PR revision:

1. A non-author Opus-role reviewer; Sam never final-gates or merges.
2. A complete peer/domain approval trail and crypto audit when applicable.
3. All GitHub checks successful, with none pending, failing, or cancelled.
4. Formatting, workspace Clippy, and the retained in-process E2E journeys passing
   locally. Run the full `./cli-test` for behavior/integration changes, including
   networking, consensus, runtime, and program changes.
5. `final_approved` recorded before merge, and `merged` after a successful merge.

No self-merge. If blocked for more than three cycles, append a context summary
to `blockers.log` and continue other authorized work. New commits require review
of the changed diff and fresh relevant validation; prior approvals/results are
not proof for an unreviewed revision. This protocol adds no deployment approval.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
./cli-test
npm ci
npm run build:web
```

The maintained tests are E2E-only. Extend meaningful retained journeys for
behavioral changes. Do not restore removed unit/property/doctest or JS/Python
unit runners. Web builds are static checks; fuzz/benchmarks remain separate.
Compose startup plus an RPC curl is not a substitute for the four-process E2E
journey. Follow [testing](docs/testing.md) for the runner, CI scope, and evidence
boundaries; report commands and outcomes without asserting WAN/provider proof.

## Shell helpers

Sourcing the launcher exposes helpers without starting the crew. It does set
shell variables/environment, so use a separate Bash process when appropriate.
The comms directory must already exist before writing ledger/assignment files.

```bash
source ./run-claude.sh
ledger_append <pr> <state> <agent_id> "<message>"
ledger_state <pr>
ledger_summary
route_domain_reviewer <pr>
requires_crypto_audit <pr>
assign <to_agent> <from_agent> "<title>" "<why>" [ref1 ref2 ...]
assignments_for <agent_id>
assignment_update <id> <state> "<note>"
thread_post <pr> <agent_id> "<message>"
thread_read <pr>
```

Assignments are append-only JSONL in `assignments.jsonl`, with `id`, `from`,
`to`, `title`, `why`, `refs`, `state`, and `priority`. Drain open assignments
before taking new work. Advance `open → accepted → in_progress → done`, or
`declined` with a reason. Supply concrete file/PR references and a problem
statement when delegating.

Per-PR dialogue lives in `threads/pr-<number>.log`. Read threads you authored or
are tagged in each cycle. State substantive review reasoning, questions, and
outcomes there; use your first name and never rubber-stamp. Keep comms actions
within the authorization for the active crew run.

## Comms files and cycle

| File under `COMMS_DIR` | Purpose |
| --- | --- |
| `general.log` | Team status, announcements, reflections |
| `claims.log` | Work claims to avoid conflicting edits |
| `completed.log` | Completed work and PR references |
| `reviews.log` | Legacy review log |
| `blockers.log` | Blockers and context summaries |
| `pr_ledger.jsonl` | PR review transitions |
| `assignments.jsonl` | Delegated work transitions |
| `threads/pr-<number>.log` | PR-specific discussion |

Each cycle: preserve checkout state; read inbox, ledger, and relevant threads;
address feedback/review queues; claim one current task; implement and validate;
register the PR/review outcome and leave a concise handoff. Agent 5 prioritizes
cryptography/architecture work in the current queue. Use [PROGRESS.md](PROGRESS.md)
for durable cross-cycle summaries, not long historical audits.
