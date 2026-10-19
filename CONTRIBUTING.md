# Contributing to Arrha / Aether

Use a topic branch or isolated checkout. Preserve other contributors' work and
keep changes reviewable. Rust requires 1.86+; web builds require Node.js 20+.

## Validation

```bash
./scripts/lint.sh
./cli-test
npm ci && npm run build:web  # for SDK/UI changes
```

`./cli-test --in-process-only` runs the faster multi-node and settlement library
journeys. The default also runs the four-process devnet with real networking,
RPC, CLI transfers, contracts, validator failure, and recovery. See
[testing](docs/testing.md) for the evidence boundaries.

Add or extend an E2E journey when behavior changes. Keep the test suite focused
on complete flows; do not reintroduce unit/property/JS/Python test runners.
Use build checks for compile/type correctness, benchmarks for performance,
and fuzzing as a separate exploration tool.

## Documentation and review

Trace architecture connections from the executable entrypoint and callers.
A crate's presence does not establish node integration or production readiness.
Update the README, architecture, command documentation, and CI together when
those contracts change. Keep future designs explicitly labeled.

PR descriptions should explain behavior, validation actually executed, and
remaining limitations. Use conventional commit subjects such as
`fix(consensus): ...` or `docs(architecture): ...`. Do not publish packages,
roll out infrastructure, or merge without the relevant authorization.
