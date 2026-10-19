# Arrha / Aether work queue

Use [architecture](docs/architecture.md) for current wiring and
[roadmap](IMPLEMENTATION_ROADMAP.md) for future work. This file replaces the
obsolete phase checklist and developer-machine paths.

- [ ] Qualify multi-validator behavior over a WAN and under sustained faults.
- [ ] Independently review consensus, cryptography, runtime, and persistence.
- [ ] Integrate parallel block execution with an E2E journey before enabling it.
- [ ] Connect finalized node events to a remote indexer/stream.
- [ ] Implement and validate attested worker → verification → settlement.
- [ ] Wire agent schema/authorization/escrow libraries into node dispatch and
      matching ecosystem producer/consumer integrations.
- [ ] Validate operator deployment assets and design tagged release publication.

For every behavior change, extend a meaningful E2E scenario, run static checks,
and report the executed environment and remaining gates. See
[contributing](CONTRIBUTING.md) and [testing](docs/testing.md). The optional crew's
review ledger remains governed by [AGENT_COMMS.md](AGENT_COMMS.md).
