# Agent cryptography: implementation and design requirements

Source review: **2026-10-18**, implementation baseline `b5460e8` (PR #413).
The filename is retained for existing links. This is a repository implementation
map and design guidance, not a claim to rank current research or certify security.

## Current cryptographic boundaries

| Surface | Implementation | Integration boundary |
| --- | --- | --- |
| Transactions | Ed25519 primitives; ledger signature verification | Running node transaction path. |
| Validator votes / certificates | BLS12-381 signing, aggregate verification and PoP helpers in `crates/crypto/bls` | `HybridConsensus` uses stake-weighted QCs. Audit every registration/verification call site; helper availability is not universal PoP enforcement. |
| Leader eligibility | VRF implementation in `crates/crypto/vrf` | Used by hybrid consensus; RFC conformance needs executed evidence. |
| Agent wire types | `crates/agent-schema` | Domain/chain/payload-bound envelopes, authorization, receipt, payment and journal types; validation is distinct from signature verification. |
| Guardian authorization | `crates/crypto/threshold` and account-abstraction | Real FROST Ristretto255 signature verification for high-risk policies. This is not a deployed signer ceremony or node dispatch service. |
| PQ primitives | `crates/crypto/pq` | ML-DSA-65/87 signing/verification and ML-KEM-768 encapsulation/decapsulation; standalone primitives and benchmarks. |
| Hybrid agent identity | `crates/sdk/rust/src/agent_identity.rs` | Ed25519 + ML-DSA-87 signing/verification helpers; not a migrated consensus or transport handshake. |
| Attestation | `crates/verifiers/tee/src/attestation.rs` | Freshness/measurement checks exist. Non-simulation reports are rejected because cryptographic certificate-chain verification is unimplemented. |
| VCR / KZG | `crates/verifiers/vcr-validator`, `crates/verifiers/kzg-verifier`, `crates/crypto/kzg` | Separate proof/verification libraries; no assembled hardware-attested inference and settlement service. |

Algorithm tags are not guarantees of implemented cryptography. For example,
`SigningAlgorithm` can name SLH-DSA while the PQ implementation supports ML-DSA
and ML-KEM; a schema variant must not be advertised as an available signer.
Likewise, KES library code does not prove operational validator key rotation.

## Schema and signing rules

Use the existing exported wire contracts and builders rather than copying a
pseudocode envelope. The concrete `SignatureEnvelope` has `alg`, `domain`,
`chain_id`, `key_id`, `payload_hash`, `signature` and optional `pq_signature`.
Versioning is encoded in the applicable domains and wire contract; there is no
universal standalone `version` field in this struct.

The schema exports explicit domains for authorization, payment and receipt
signatures. Preserve canonical encoding, typed hashing and exact field binding
between producer and verifier. Do not substitute a generic hash recipe for
`typed_hash` or the concrete signing-payload methods.

Security review should trace all of the following through an actual journey:

- Which fields are signed, which are merely structural metadata, and which
  trusted key registry establishes the signer's authority.
- Chain/domain/payload matching, signature algorithm selection and hybrid
  verification that rejects when either required component fails.
- Session expiry, allowed side effects/tools/recipients and spend limits.
- Guardian authorization bound to the operation's policy hash, including payment
  terms, rather than a reusable generic approval.
- Nonce and spend accounting that survive restart, concurrent operations and
  failed calls without allowing replay or exceeding caps.
- Receipt order, previous-hash chaining and journal inclusion proofs; evidence
  authenticity and availability remain separate from hash inclusion.
- Payment binding to request, quote, result, recipient, token, amount, nonce,
  expiry and side-effect policy at the consuming service boundary.

## Threshold and post-quantum requirements

FROST verification is implemented using a Ristretto255 suite. Distributed key
generation, fresh one-time signing nonces, share custody, participant enrollment,
abort handling, recovery and rotation need their own integration and operational
evidence. A valid aggregate signature does not prove safe signer operations.

Keep consensus BLS and transaction Ed25519 behavior explicit. ML-DSA helpers
support hybrid agent identity experiments; ML-KEM helpers do not make current
TCP/Noise transport post-quantum secure. A future hybrid handshake must bind
both component secrets, peer identities, negotiated algorithms and transcript,
with downgrade/replay protection and compatible key lifecycle procedures.

Benchmark primitives before setting verification budgets or changing hot-path
algorithms. Retained PQ benchmarks can be invoked independently:

```bash
cargo bench -p aether-crypto-pq
```

Benchmark availability is not a measured result in this documentation update.

## Attestation and proof requirements

Use the RATS separation between evidence, verification policy and attestation
results when implementing a real hardware path. Require trusted vendor roots,
cryptographic quote/certificate verification, approved measurements, nonce-bound
freshness, revocation and explicit production rejection of simulation reports.
The current verifier deliberately refuses unsupported non-simulation trust
checks; do not weaken it to claim a successful hardware journey.

A signed receipt or TEE measurement alone cannot establish user consent or the
meaning of an external browser/API side effect. Define challenge evidence,
human review, timeout/refund policy and who can authorize the protected action.
zkVM/zkML spot checks, FHE and reputation oracles are optional future designs;
this repository does not establish their production soundness, cost or coverage.

## Current E2E evidence and remaining gates

```bash
cargo test -p aether-sdk --test agent_settlement_e2e
```

The retained cross-library E2E exercises SDK authorization and identity,
receipts, account policy, signatures and escrow settlement transitions. It
operates in-process. It does not prove hardware inference, remote signing,
sibling interoperability, actual RPC settlement or production readiness.

The maintained [testing policy](../testing.md) is E2E-only; former unit,
property and vector suites are not runnable acceptance gates. Add adversarial
binding/expiry/replay and guardian scenarios to real journeys when needed.
Conformance with a standard must be demonstrated using exact input/output
vectors or equivalent evidence within those journeys; a similar algorithm name
is insufficient.

Before a deployed integration is claimed, require a pinned producer/consumer
journey, finalized balance movement and restart persistence, wrong-chain and
replay rejection, guardian lifecycle evidence and an external security review.
See [ecosystem settlement](../ecosystem/aether-agent-settlement.md),
[threat model](THREAT_MODEL.md) and [audit scope](AUDIT_SCOPE.md).

## Standards references for implementation review

These define relevant primitives and architecture. Listing a standard is not a
claim that the implementation is certified or conformant.

- [FIPS 203: ML-KEM](https://csrc.nist.gov/pubs/fips/203/final).
- [FIPS 204: ML-DSA](https://csrc.nist.gov/pubs/fips/204/final).
- [FIPS 205: SLH-DSA](https://csrc.nist.gov/pubs/fips/205/final) (not implemented here).
- [RFC 9591: FROST](https://www.rfc-editor.org/rfc/rfc9591.html).
- [RFC 9381: VRFs](https://www.rfc-editor.org/rfc/rfc9381.html).
- [RFC 9334: RATS architecture](https://www.rfc-editor.org/rfc/rfc9334.html).
