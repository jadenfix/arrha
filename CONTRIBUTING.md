# Contributing to Arrha / Aether

Use a topic branch or isolated checkout. Preserve other contributors' work and
keep changes reviewable. Use current stable Rust (CI's toolchain); the workspace
package metadata declares Rust 1.86. Web builds require Node.js 20+.
See [Getting Started](GETTING_STARTED.md) for native build dependencies.

## Validation

```bash
./scripts/lint.sh
./cli-test
npm ci && npm run build:web  # for SDK/UI changes
```

Run commands from the repository root. `./scripts/test.sh` is an alias for
`./cli-test`, not a second test suite. Lint performs formatting, Clippy, and a
workspace compile check; the web build compiles the SDK and UI workspaces.

`./cli-test --in-process-only` runs the faster multi-node and settlement library
journeys. The default also runs the four-process devnet with real networking,
RPC, CLI transfers, contracts, validator failure, and recovery. The process suite
uses ports 8545–8548 and a disposable `data/e2e-devnet` directory. Stop conflicting
networks first; use `KEEP_DEVNET_DATA=1 ./cli-test` to retain its logs/state. See
[testing](docs/testing.md) for the evidence boundaries.

Add or extend an E2E journey when behavior changes. Keep the test suite focused
on complete flows; do not reintroduce unit/property suites. The independent `settlement/` package
uses Node's built-in runner for complete deployed EVM/service/browser journeys;
it does not replace the original L1 E2E policy.
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

## Choose the right integration surface

For node-facing examples, use `aetherctl chain` or the Rust SDK's signed chain
helpers, and include a finality/receipt check. Treat TypeScript/Python job builders,
settlement libraries, the firehose, and deployment assets according to their actual
callers and transport wiring. A successful library E2E scenario does not establish a
running worker, public API, or deployed network.

Keep measured benchmark results tied to their original revision, hardware, command,
and raw artifacts. Record new measurements separately rather than attributing old
numbers to the current branch. For documentation changes, check local links and run
self-contained examples where practical; do not label source review as executed E2E.
