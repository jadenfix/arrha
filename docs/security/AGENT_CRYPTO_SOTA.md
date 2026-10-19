# Aether agent cryptography SOTA report

> Scope checked 2026-10-18: this is threat-model/design guidance, not an audit
> certificate or a list of fully integrated services. Current executable wiring
> is in [architecture](../architecture.md); executed checks must be reported
> separately. The maintained test suite is [E2E-only](../testing.md). Historical
> unit/property/vector-test references below are requirements or prior evidence,
> not current runnable test targets. AI workers and remote signers require their
> own implementation/deployment evidence.

This report defines the cryptographic architecture Aether should use as the settlement layer for Beater ecosystem agents. It is written for implementation planning, not marketing. The recommendation is intentionally layered: no single primitive can simultaneously provide low-latency consensus, agent wallet authorization, long-lived identity, private tool execution, verifiable inference, and machine-web payments.

## Executive recommendation

Aether should keep the current hot-path L1 crypto where it is already the right tool, then add an agent-specific crypto layer above it:

| Surface | Best default | Why |
| --- | --- | --- |
| Consensus votes and light-client finality | BLS12-381 signatures with proof-of-possession and duplicate-signer rejection | This remains the best deployed primitive for compact quorum certificates and light-client proofs. |
| Slot leader election | RFC 9381-style ECVRF over Edwards25519, with strict test-vector conformance | VRFs are the right primitive for private, publicly verifiable leader eligibility. |
| Transaction/user signatures | Ed25519 for v1, with a versioned signature envelope | Fast, mature, small signatures; do not mix this with consensus BLS. |
| Agent session keys and human co-signing | FROST over Ristretto255/Ed25519-compatible suites where threshold signing is needed | Threshold authorization prevents one hot agent key from controlling money or high-risk side effects. |
| Long-lived identity and governance checkpoints | Hybrid Ed25519 plus ML-DSA; SLH-DSA for cold emergency roots | Long-lived identities must be post-quantum migratable now, even if the fast path stays classical. |
| Transport/session secrecy | Hybrid X25519 plus ML-KEM-768 for node, worker, and bridge handshakes | Store-now-decrypt-later risk matters for agent journals and settlement evidence. |
| LLM inference verification | TEE remote attestation plus VCR today; ZK spot checks for high-value disputes | zkML is improving, but full billion-parameter inference proofs are still too expensive for the common path. |
| Browser/tool/sandbox execution | Signed receipts plus Merkle journal roots plus optimistic challenges | These actions are interactive and side-effectful; cryptographic replay alone cannot prove user intent or external-world semantics. |
| Paid HTTP/API actions | x402-compatible AIC payment envelopes, but bound to request hash, quote hash, nonce, expiry, chain id, and SideEffect policy | The trend is HTTP-native agent payments, but recent research highlights replay and binding risks if the payment is not tightly tied to the service result. |

The design principle is: use aggregation where validators need many signatures, use threshold signatures where agents delegate authority, use post-quantum hybrids for long-lived identity and confidential transport, and use optimistic/TEE/ZK verification according to the economics of the step.

## First-principles constraints

1. Agents are principals with delegated authority, not merely API clients. A compromised agent hot key must not be able to spend unlimited AIC or perform Purchase/Delete actions.
2. Settlement must be verifiable without putting full execution on-chain. The chain should store commitments, signatures, attestations, bonds, and dispute outcomes.
3. Latency matters. Aether targets sub-2s finality, so every validator vote cannot carry large post-quantum signatures in the consensus hot path.
4. Durability matters. Agent identities, reputation, escrow history, governance votes, and dispute evidence may remain valuable after quantum-capable adversaries exist.
5. External side effects are not pure computation. A browser click, email send, purchase, or delete cannot be proven correct by a hash alone; it needs policy-bound authorization and challengeable evidence.
6. Crypto agility is mandatory. Every signed object needs an algorithm identifier, domain separator, version, chain id, and typed payload hash so that Aether can add ML-DSA, SLH-DSA, FROST, or new zk verifiers without replay ambiguity.

## Scientific and standards basis

### Post-quantum cryptography

NIST finalized the first PQC standards on August 13, 2024:

- FIPS 203 standardizes ML-KEM, with ML-KEM-512, ML-KEM-768, and ML-KEM-1024 parameter sets. NIST states ML-KEM is believed secure even against quantum adversaries and is intended for shared-secret establishment.
- FIPS 204 standardizes ML-DSA for digital signatures and states it is believed secure against large-scale quantum computers.
- FIPS 205 standardizes SLH-DSA, a stateless hash-based signature algorithm based on SPHINCS+.
- NIST SP 800-208 also recommends stateful LMS/XMSS/HSS/XMSS-MT. These are appropriate only for tightly controlled cold signing because state misuse can be catastrophic.

Implication for Aether: do not replace BLS or Ed25519 in the hot path immediately. Instead, add versioned hybrid envelopes:

```text
SignatureEnvelope {
  alg: ed25519 | bls12_381 | frost_ristretto255 | ed25519_mldsa87 | slhdsa_sha2_256f
  domain: "aether/<object>/<version>"
  chain_id
  public_key_id
  payload_hash
  signature
  pq_signature_optional
}
```

For long-lived identity, governance, genesis, validator registration, and agent credential roots, require either a hybrid signature now or a migration path with dual registration. For high-frequency transactions and receipts, keep Ed25519 until ML-DSA verification costs are measured in Aether's WASM/runtime and light-client contexts.

### Threshold authorization

RFC 9591 specifies FROST, a two-round Schnorr threshold signing protocol with ciphersuites for Ed25519, Ristretto255, P-256, secp256k1, and Ed448. It is the right shape for human+agent authorization because it distributes trust across keys and supports identifiable aborts and standard signature verification properties.

Implication for Aether:

- Use FROST for AA guardian approvals, validator operational ceremonies, and high-risk agent actions.
- Prefer Ristretto255 internally for prime-order group hygiene; use Ed25519-compatible signatures only where ecosystem compatibility requires it.
- Never use deterministic nonces for multiparty signing. FROST explicitly requires fresh nonce handling; nonce reuse leaks signing shares.
- Do not use threshold BLS as the default agent wallet primitive. BLS is already valuable for validator aggregation, but FROST maps better to session-key and guardian-account semantics.

### Consensus aggregation

BLS remains the best practical signature primitive for compact validator quorum certificates. Aether already uses BLS12-381 with proof-of-possession and recent fixes for duplicate signer rejection and batch verification. That should remain the consensus and light-client path.

Rules:

- Keep BLS scoped to consensus/finality and possibly committee attestations.
- Require proof-of-possession for registered validator keys.
- Bind all signed consensus messages to chain id, domain, role, slot/height, and fork digest.
- Do not use BLS for agent spend signatures unless there is a concrete aggregation requirement.

### Remote attestation

RFC 9334 defines the RATS architecture: an Attester produces Evidence, a Verifier appraises it using policies and endorsements, and the Relying Party consumes Attestation Results. It explicitly includes confidential machine-learning model protection and critical-control use cases.

Implication for Aether:

- Model TEE verification as RATS-style Evidence -> AttestationResult -> on-chain commitment, not as raw vendor quote parsing inside every program.
- Store vendor-neutral attestation result hashes on-chain and full evidence off-chain.
- Require freshness through nonces, epochs, or challenge-bound quotes.
- Treat TEE as an integrity and confidentiality accelerator, not as the sole source of truth for settlement. Pair it with optimistic disputes and selective ZK proofs.

### ZK, zkVM, zkML, and FHE

Recent arXiv work supports a pragmatic split:

- ZKML surveys through 2024 show active progress in verifiable training, inference, and testing, but also substantial implementation and performance barriers.
- 2025-2026 zkVM work shows Rust/C programs can increasingly be proven in general-purpose zkVMs, but zkVM soundness/completeness bugs are real and require systematic testing.
- Optimistic TEE-rollup research for generative AI argues that full zkML for billion-parameter inference is not yet the low-latency common path; TEE provisional finality plus fraud proofs and stochastic ZK checks is a more realistic architecture.
- FHE and hybrid homomorphic encryption are valuable for private computation but remain too expensive and specialized for default agent settlement.

Implication for Aether:

- Use zkVM proofs first for deterministic sandbox receipts, small policy checks, fraud proofs, and reproducible build/provenance proofs.
- Use zkML only for high-value model classes or spot-check challenges, not every LLM token stream.
- Use FHE only for privacy-preserving analytics or specialized paid services; do not make it a v1 settlement dependency.

### Agent identity and payments

The current trend is converging on ledger-anchored identities, verifiable credentials, account abstraction, and HTTP-native payments:

- W3C Verifiable Credentials 2.0 and DID-style systems provide portable identity and attestations.
- ERC-4337, ERC-7579, and EIP-7702 show where smart-account and modular-account ecosystems are going.
- x402 is now an active HTTP-native payment standard for agents and API services, but recent papers on x402 attacks and atomic service channels show that payment payloads must be bound to the exact resource, result, nonce, expiry, and service terms.

Implication for Aether:

- Aether should not clone Ethereum account abstraction wholesale, but should preserve the concepts: UserOperation-like typed actions, validator modules, session keys, paymasters, and policy modules.
- The AIC payment envelope should be x402-compatible at the HTTP layer while settling natively on Aether.
- For paid actions, bind the payment authorization to request hash, quote hash, result hash or result commitment, method/path/resource id, recipient, amount and token, nonce, expiry, chain id, SideEffect class, and max replay count.

## Recommended Aether crypto stack

### Layer 0: typed object hashing

Every cryptographic object should use typed, domain-separated hashing:

```text
hash = BLAKE3-256(
  "aether:" || object_kind || ":" || version || ":" ||
  chain_id || canonical_borsh_or_json_cbor_payload
)
```

SHA-256 remains acceptable for interoperability and existing Merkle paths. BLAKE3 is appropriate for internal high-throughput content hashing. The important property is not the specific hash alone; it is stable canonical encoding plus explicit domain separation.

### Layer 1: consensus and validator crypto

Keep:

- BLS12-381 signatures for votes, aggregate QCs, and light-client finality.
- ECVRF-Edwards25519-SHA512 for leader election.
- KES for validator operational key evolution if already part of the slashing/threat model.

Add:

- Validator registration must include classical BLS PoP plus optional PQ identity signature.
- Consensus message domains must be audited and frozen.
- A benchmark budget for future PQ-finality experiments, but no v1 replacement of BLS.

### Layer 2: account abstraction and agent authority

Add an `agent-auth` design:

```text
AgentAuthorization {
  agent_account
  session_public_key
  delegated_by
  valid_from_slot
  valid_until_slot
  max_aic
  max_per_call_aic
  allowed_side_effects
  allowed_tools
  allowed_recipients
  policy_hash
  guardian_threshold
  signature_envelope
}
```

Rules:

- Read/Draft can use hot Ed25519 session signatures.
- Write requires idempotency key plus bounded session key.
- Send/Purchase/Delete require guardian approval: FROST threshold or multisig.
- Long-lived account roots should be hybrid Ed25519+ML-DSA.

### Layer 3: receipts and journals

Each agent step should produce a signed receipt:

```text
StepReceipt {
  run_id
  seq
  prev_receipt_hash
  kind
  side_effect
  request_hash
  result_hash
  evidence_uri_hash
  tool_identity
  signer
  signature_envelope
}
```

The journal root should be a Merkle Mountain Range or append-only Merkle tree over receipt hashes. `prev_receipt_hash` prevents reorder ambiguity in off-chain streaming logs; the Merkle root gives efficient dispute proofs.

### Layer 4: verification tiers

| Step | Default tier | Escalation tier |
| --- | --- | --- |
| LLM inference | TEE attestation + VCR + optimistic challenge | zkML spot check or rerun committee |
| Deterministic sandbox | Signed beatbox result + WASM/fuel hash | zkVM proof for disputed result |
| Browser action | tempo receipt + observation hash + cassette evidence | human/arbiter challenge |
| MCP/API call | signed HTTP transcript hash + x402/AIC payment proof | remote service attestation or dispute |
| High-risk side effect | guardian signature + receipt + challenge window | manual arbitration before settlement |

## Implementation sequence

1. Add `aether-agent-schema` with `SignatureEnvelope`, `AgentAuthorization`, `StepReceipt`, `JournalRoot`, and `PaymentEnvelope`.
2. Add `crates/crypto/pq` behind a feature flag for ML-DSA and ML-KEM benchmarking. The initial requirement is schema and tests, not consensus integration.
3. Add `crates/crypto/threshold` for FROST-compatible threshold authorization. Start with Ristretto255 and test vectors.
4. Extend account abstraction with session-key and SideEffect policy validators.
5. Extend `agent-run-escrow` to require policy-bound receipt hashes for settlement.
6. Add bridge conformance tests with beater.js, tempo, beatbox, and beater.
7. Add audit gates for canonical encoding, domain separation, replay resistance, nonce handling, and key lifecycle.

### Current implementation map

The first implementation slice is intentionally split into standalone crates so other ecosystem repos can adopt the agent crypto layer without linking the full node:

| Boundary | Crate/API | Current status |
| --- | --- | --- |
| Wire schema | `aether-agent-schema` | Implements crypto-agile signature envelopes, agent authorizations, step receipts, journal inclusion proofs, payment envelopes, typed BLAKE3 hashes, and JSON Schema export. |
| Threshold approvals | `aether-crypto-threshold` | Verifies real FROST Ristretto255 guardian signatures for high-risk account-abstraction actions. |
| PQ identity/transport readiness | `aether-crypto-pq` | Implements FIPS 204 ML-DSA-65/87 signing and verification plus FIPS 203 ML-KEM-768 encapsulation/decapsulation with fixed-size boundary validation and Criterion benchmarks. |
| SDK adoption | `aether-sdk::agent_identity` and `aether-sdk::agent_builder` | Provides hybrid Ed25519+ML-DSA-87 agent identity signing/verification and builders for authorization, receipt, and payment envelopes. |
| Policy enforcement | `aether-account-abstraction` | Rejects high-risk `Send`, `Purchase`, and `Delete` operations unless the operation policy hash is FROST-approved by guardians. |
| Settlement | `aether-program-agent-run-escrow` | Anchors run budgets, receipt hashes, journal roots, challenge windows, settlement, disputes, and refunds. |

The retained cross-library E2E gate is:

```bash
cargo test -p aether-sdk --test agent_settlement_e2e
```

It exercises SDK authorization, receipts, account policy, signatures, and escrow
settlement. The former per-crate unit/property/vector suites were removed under
the E2E-only policy. This gate is not a claim of hardware-attested inference,
a remote signer, sibling integration, or deployed node settlement. PQ benchmark
sources remain separate performance tools.

## What not to do

- Do not replace all signatures with PQ signatures immediately. That would harm latency and light-client ergonomics before there is a measured threat/benefit tradeoff.
- Do not use TEEs as the only settlement oracle. TEEs need remote attestation, freshness, policy appraisal, and challenge paths.
- Do not make full zkML a v1 dependency. Use it selectively where the proving cost is justified.
- Do not put full journals, browser states, or traces on-chain. Anchor commitments and evidence hashes.
- Do not let x402-style payments be generic bearer receipts. Bind every payment to the exact service request and SideEffect policy.

## Acceptance criteria

| Gate | Requirement |
| --- | --- |
| Crypto schema | Every signed object has algorithm id, domain, version, chain id, payload hash, and canonical encoding tests. |
| PQ readiness | ML-DSA and ML-KEM benchmark harness exists; long-lived identity records can carry hybrid signatures. |
| Threshold auth | FROST test vectors pass; guardian approval required for Send/Purchase/Delete in AA tests. |
| Receipt security | StepReceipt has prev-hash chaining, Merkle inclusion proofs, replay tests, and signer-policy validation. |
| Payment security | AIC/x402 envelope binds quote, request, result, nonce, expiry, recipient, amount, and SideEffect. |
| Verification economics | Each StepKind has documented default verification tier, challenge tier, bond, and timeout. |
| Audit scope | External audit covers BLS PoP, VRF conformance, FROST nonce safety, PQ hybrid envelopes, receipt replay, and payment binding. |

## Sources

- NIST FIPS 203, Module-Lattice-Based Key-Encapsulation Mechanism Standard: https://csrc.nist.gov/pubs/fips/203/final
- NIST FIPS 204, Module-Lattice-Based Digital Signature Standard: https://csrc.nist.gov/pubs/fips/204/final
- NIST FIPS 205, Stateless Hash-Based Digital Signature Standard: https://csrc.nist.gov/pubs/fips/205/final
- NIST SP 800-208, Recommendation for Stateful Hash-Based Signature Schemes: https://csrc.nist.gov/pubs/sp/800/208/final
- RFC 9591, FROST threshold Schnorr signatures: https://www.rfc-editor.org/rfc/rfc9591.html
- IETF CFRG BLS signatures draft: https://datatracker.ietf.org/doc/draft-irtf-cfrg-bls-signature/
- RFC 9381, ECVRF: https://www.rfc-editor.org/rfc/rfc9381.html
- RFC 9334, RATS architecture: https://www.rfc-editor.org/rfc/rfc9334.html
- W3C Verifiable Credentials Data Model v2.0: https://www.w3.org/TR/vc-data-model-2.0/
- ERC-4337 Account Abstraction: https://eips.ethereum.org/EIPS/eip-4337
- ERC-7579 Minimal Modular Smart Accounts: https://eips.ethereum.org/EIPS/eip-7579
- EIP-7702 Set Code for EOAs: https://eips.ethereum.org/EIPS/eip-7702
- x402 overview: https://docs.cdp.coinbase.com/x402/welcome and https://x402.org/
- A Survey of Zero-Knowledge Proof Based Verifiable Machine Learning, arXiv:2502.18535: https://arxiv.org/abs/2502.18535
- Evaluating Compiler Optimization Impacts on zkVM Performance, arXiv:2508.17518: https://arxiv.org/abs/2508.17518
- Arguzz: Testing zkVMs for Soundness and Completeness Bugs, arXiv:2509.10819: https://arxiv.org/abs/2509.10819
- Verifiable Provenance of Software Artifacts with Zero-Knowledge Compilation, arXiv:2602.11887: https://arxiv.org/abs/2602.11887
- Optimistic TEE-Rollups, arXiv:2512.20176: https://arxiv.org/abs/2512.20176
- Towards Multi-Agent Economies, arXiv:2507.19550: https://arxiv.org/abs/2507.19550
- Five Attacks on x402 Agentic Payment Protocol, arXiv:2605.11781: https://arxiv.org/abs/2605.11781
- A402 Atomic Service Channels, arXiv:2603.01179: https://arxiv.org/abs/2603.01179
