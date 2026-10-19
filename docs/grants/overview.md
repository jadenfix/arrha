# Local faucet and scorecard tools

These tools support local development. This repository does not establish a
live public faucet, funded grant/bounty program, weekly publisher, or automatic
validator rewards.

## Faucet request service

Use the loopback-bound helper from the repository root:

```bash
./scripts/run_faucet_local.sh
```

The helper defaults to `127.0.0.1:8080`. Running `cargo run -p aether-faucet
--bin server` directly instead defaults to `0.0.0.0:8080`.

```bash
curl -fsS http://127.0.0.1:8080/request \
  -H 'Content-Type: application/json' \
  -d '{"github":"local-dev","address":"0x1111111111111111111111111111111111111111","token":"AIC","amount":1000}'
```

[`POST /request`](../../crates/tools/faucet/src/lib.rs) validates a supplied
handle, address, token, and amount, then returns grant metadata. It does not sign
or submit a transaction, connect to node RPC, verify GitHub identity, or fund an
account. Rate-limit state is in memory and resets when the service restarts.

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `AETHER_FAUCET_ADDR` | Helper: `127.0.0.1:8080`; binary: `0.0.0.0:8080` | Listener address |
| `AETHER_FAUCET_LIMIT` | 250000 | Maximum and omitted-request amount |
| `AETHER_FAUCET_COOLDOWN` | 600 | Seconds between accepted requests for the supplied handle |

The default token allowlist is `AIC` and `SWR`, matched case-insensitively.
These labels do not establish integrated token funding. See [server
configuration](../../crates/tools/faucet/src/bin/server.rs). Complete node
funding integration and an executed E2E journey before offering a public faucet.

## Scorecards

[The scorecard crate](../../crates/tools/scorecard/src/lib.rs) converts an array
of supplied validator samples into ranked Markdown/CSV. Each sample requires
`identity` and percentage `uptime`; `avg_latency_ms`, `finality_faults`, and
`missed_slots` default to zero when omitted. The score deducts penalties for
uptime below 100%, latency above 150 ms, finality faults, and missed slots.

```bash
cargo run -p aether-scorecard --bin scorecard -- --help
./scripts/generate_scorecard.sh metrics.json out/scorecard.md out/scorecard.csv
```

Provide your own `metrics.json` array. The helper creates output directories;
the binary accepts `--input`, `--markdown-out`, and `--csv-out`. Without a
Markdown output path, the binary prints the table to stdout.

Scores describe supplied input. No live metric collection, publication
schedule, grant eligibility, or automatic payment is implied. See
[testing](../testing.md) and [architecture](../architecture.md) for integration
and evidence boundaries.
