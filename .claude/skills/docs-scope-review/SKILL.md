---
name: docs-scope-review
description: End-to-end documentation and scope iteration for jadenfix/arrha. Use when writing, reviewing, or finalizing README sections, ecosystem docs, architecture docs, PR bodies, roadmaps, specs, public contracts, generated-client-facing docs, or any doc cleanup that must stay aligned with runtime behavior and repository scope.
---

# Arrha / Aether docs and scope review

Use this skill to make documentation changes complete, honest, and merge-ready. The goal is not prettier prose; the goal is a durable public contract that matches the code, PR scope, and ecosystem boundaries.

## Core rules

- Derive scope from authoritative evidence: issue, PR body, diff, runtime code, schemas, generated-client surfaces, existing docs, and linked ecosystem docs.
- Keep docs honest. Separate what ships now from roadmap, design intent, and optional integrations.
- Preserve standalone value. Aether must read as useful by itself; ecosystem links are opt-in protocol boundaries, not hidden dependencies.
- Update all public descriptions in the same slice when behavior changes: README, architecture docs, OpenAPI/generated-client-facing docs, SDK docs, examples, runbooks, and PR body.
- Do not publish opaque integration claims. A connection exists only when a real producer and consumer exist, with a named protocol artifact between them.
- When moving docs, rewrite links from the new location. Repo-local links should resolve in the repo; sibling-project links should use stable GitHub URLs unless the repository intentionally vendors that sibling.
- Prefer the smallest doc set that proves the invariant. Remove stale duplicate docs instead of preserving competing sources of truth.

## Current source and validation references

Use `docs/architecture.md` for executable wiring, `docs/testing.md` for the E2E-only
policy, and `.github/workflows/ci.yml` for configured CI. The repository is
`jadenfix/arrha`; Rust packages and binaries still use Aether. Do not restore
removed phase plans, unit/property suites, or competing overview documents.
Historical benchmark results keep their original revisions and environments.

## Workflow

1. Resolve the target: PR, issue, branch, or file set.
2. State the doc contract in one sentence: what reader decision should this doc make safer or faster?
3. Inventory affected public surfaces: README, overview, architecture, security, API schemas, SDK docs, generated clients, runbooks, ecosystem docs, PR title/body.
4. Check consistency across surfaces:
   - Runtime behavior vs docs.
   - Status codes, routes, fields, schemas, CLI flags, environment variables, and examples.
   - Standalone scope vs ecosystem integration scope.
   - Current implementation vs future work.
5. Edit toward a single information hierarchy:
   - README: short orientation and link to detail.
   - Detail doc: precise scope, gaps, dependency graph, non-goals, definition of done.
   - PR body: what changed, why, verification status, residual risks.
6. Remove or archive stale docs when they would mislead the next agent.
7. If asked to review, findings come first with file:line evidence and a concrete reader or runtime failure.

## Definition of done for docs

- A new reader can identify what exists, what is planned, and what is explicitly out of scope.
- All links introduced or moved have a reason and resolve from their final location.
- Public contract changes are mirrored in generated-client-facing docs or schemas when applicable.
- PR title/body describe the real scope, not only cleanup mechanics.
- The docs make the system easier to adopt because they reduce ambiguity, not because they add volume.
