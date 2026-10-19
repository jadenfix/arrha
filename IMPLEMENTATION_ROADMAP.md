# Arrha / Aether delivery roadmap

Current source wiring is documented in [architecture](docs/architecture.md).
The local node supports transfers and WASM contracts, multi-validator consensus,
and finalized persistence. These are prototype capabilities, not mainnet qualification.

## Protocol and deployment

- Exercise long-running WAN networks, partition/recovery, validator churn, and
  adversarial loads with reproducible E2E evidence.
- Review consensus, cryptographic, runtime, and storage boundaries independently.
- Validate Compose/Helm/Kubernetes/Terraform environments before documenting
  them as supported deployments.
- Design reproducible tagged artifacts and operator-reviewed rollouts; CI currently
  builds but does not publish or deploy.

## Component integration

- Wire and validate parallel scheduling before claiming parallel block execution.
- Wire QUIC/Turbine/DA only with an executed multi-node journey.
- Connect finalized node events to a real remote streaming/indexer service.
- Implement an attested worker executable and node-facing verification/settlement
  integration. The old print-only runtime and nonfunctional model build assets
  were removed; the router/coordinator/worker and verifier libraries remain.
- Complete node dispatch and ecosystem producer/consumer bridges for agent
  settlement. Schema, SDK, authorization, and escrow libraries are available;
  library E2E does not establish public deployment.

## Developer workflow

Maintain an E2E-only test suite, static Rust/web builds, and source-linked docs.
Add browser/provider journeys when those systems become operational. Preserve
benchmark and fuzz tools separately, with explicit environment and evidence limits.
