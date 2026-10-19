# Local faucet and scorecard tools

These are development tools. This repository does not establish a live public
faucet, funded grant/bounty program, weekly publisher, or automatic rewards.

## Faucet

```bash
cargo run -p aether-faucet --bin server
```

The server exposes `POST /request` and local rate-limiting/grant logic. Review
`crates/tools/faucet/src/bin/server.rs` and `src/lib.rs` for the actual configuration
and request types. A grant response does not by itself establish a finalized
on-chain transfer. Do not treat a supplied GitHub handle as verified identity.

The helper `scripts/run_faucet_local.sh` starts the local service. Complete node
funding integration and an E2E journey before advertising a public faucet.

## Scorecards

`crates/tools/scorecard` converts supplied metrics snapshots into Markdown/CSV:

```bash
cargo run -p aether-scorecard --bin scorecard -- --help
```

`scripts/generate_scorecard.sh` is a local helper. Scores describe the supplied
input; no live network, publication schedule, grant eligibility, or automated
payment is implied. See [testing](../testing.md) and [architecture](../architecture.md).
