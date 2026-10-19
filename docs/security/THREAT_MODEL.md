# Aether Blockchain - Security Threat Model

> Scope checked 2026-10-18: this is threat-model/design guidance, not an audit
> certificate or a list of fully integrated services. Current executable wiring
> is in [architecture](../architecture.md); executed checks must be reported
> separately. The maintained test suite is [E2E-only](../testing.md). Historical
> unit/property/vector-test references below are requirements or prior evidence,
> not current runnable test targets. AI workers and remote signers require their
> own implementation/deployment evidence.

**Version**: 1.0  
**Date**: October 13, 2025  
**Methodology**: STRIDE + LINDDUN

## Table of Contents
1. [Executive Summary](#executive-summary)
2. [System Architecture](#system-architecture)
3. [STRIDE Analysis](#stride-analysis)
4. [LINDDUN Privacy Analysis](#linddun-privacy-analysis)
5. [Attack Surfaces](#attack-surfaces)
6. [Mitigations](#mitigations)
7. [Residual Risks](#residual-risks)

---

## Executive Summary

This document provides a comprehensive threat analysis of the Aether blockchain using STRIDE (Spoofing, Tampering, Repudiation, Information Disclosure, Denial of Service, Elevation of Privilege) and LINDDUN (Linkability, Identifiability, Non-repudiation, Detectability, Disclosure of information, Unawareness, Non-compliance) methodologies.

**Key Findings**:
- 23 identified threats across 6 STRIDE categories
- 8 high-severity threats requiring immediate mitigation
- 15 medium-severity threats with planned mitigations
- Strong cryptographic foundation mitigates most spoofing/tampering threats
- DoS and privacy concerns require ongoing attention

---

## System Architecture

### Components
1. **Consensus Layer**: VRF-PoS leader election + HotStuff BFT
2. **Networking Layer**: QUIC transport + Turbine DA propagation
3. **Execution Layer**: WASM runtime with parallel scheduler
4. **State Layer**: Sparse Merkle Tree + RocksDB
5. **AI Mesh**: TEE workers + VCR verification
6. **Economic Layer**: Staking, governance, AMM, job escrow

### Trust Boundaries
- **Validator ↔ Validator**: Authenticated QUIC, BLS signatures
- **User ↔ Validator**: Ed25519 signatures, Merkle proofs
- **AI Worker ↔ Chain**: TEE attestation, KZG commitments
- **Off-chain ↔ On-chain**: External oracles (future)

---

## STRIDE Analysis

### S - Spoofing

#### S1: Malicious Validator Impersonation
**Threat**: Attacker impersonates legitimate validator to inject invalid blocks  
**Impact**: High - Could cause chain fork or invalid state transition  
**Likelihood**: Low - Requires compromising validator private keys  
**Mitigations**:
- ✅ Ed25519 signatures on all transactions
- ✅ BLS signatures on votes
- ✅ VRF proof verification for leader election
- 🔄 KES (Key Evolving Signatures) for forward secrecy
- 🔄 Remote signer with HSM/KMS

#### S2: Sybil Attacks on P2P Network
**Threat**: Attacker creates many fake peer identities to manipulate network  
**Impact**: Medium - Could eclipse honest nodes  
**Likelihood**: Medium - Low cost to create identities  
**Mitigations**:
- ✅ Peer reputation scoring
- ✅ Stake-weighted network topology
- 🔄 Connection limits per IP/ASN
- 🔄 Proof-of-work for peer discovery

#### S3: TEE Attestation Forgery
**Threat**: Attacker forges TEE attestation to run unverified AI workers  
**Impact**: High - Compromises verifiable compute integrity  
**Likelihood**: Low - Requires breaking Intel TDX/AMD SEV-SNP  
**Mitigations**:
- ✅ Certificate chain verification
- ✅ Measurement whitelist
- ✅ Freshness checks (<60s)
- 🔄 Multiple attestation providers

---

### T - Tampering

#### T1: Block Data Tampering
**Threat**: Attacker modifies block data in transit or storage  
**Impact**: High - Invalid state transitions  
**Likelihood**: Low - Cryptographic protection  
**Mitigations**:
- ✅ Block hash chains (every block references parent)
- ✅ Merkle root commitments
- ✅ BLS aggregate signatures on finality
- ✅ QUIC with TLS 1.3 encryption

#### T2: State Database Corruption
**Threat**: Physical/malware attack corrupts RocksDB state  
**Impact**: High - Validator unable to participate  
**Likelihood**: Low - Requires host compromise  
**Mitigations**:
- ✅ Merkle proofs for state verification
- ✅ Snapshot-based recovery
- 🔄 Replicated state across validators
- 🔄 Checksums on disk writes

#### T3: Smart Contract Bytecode Modification
**Threat**: Attacker modifies WASM bytecode before execution  
**Impact**: High - Arbitrary code execution  
**Likelihood**: Low - Deterministic hash verification  
**Mitigations**:
- ✅ Content-addressable program storage
- ✅ Hash verification before loading
- ✅ WASM module validation
- ✅ Gas metering prevents infinite loops

---

### R - Repudiation

#### R1: Transaction Denial
**Threat**: User denies submitting transaction after execution  
**Impact**: Low - Auditable blockchain prevents this  
**Likelihood**: Low - All transactions are signed  
**Mitigations**:
- ✅ Ed25519 signatures provide non-repudiation
- ✅ Transaction receipts with Merkle proofs
- ✅ Immutable ledger history
- ✅ Block explorer for public audit

#### R2: Validator Equivocation Denial
**Threat**: Validator denies signing conflicting blocks  
**Impact**: Medium - Slashing disputes  
**Likelihood**: Low - Evidence is cryptographic  
**Mitigations**:
- ✅ Slashing proofs include both signatures
- ✅ BLS signature aggregation is binding
- ✅ Cryptographic evidence stored on-chain

---

### I - Information Disclosure

#### I1: Private Key Leakage
**Threat**: Validator private keys leaked via memory dump/logs  
**Impact**: Critical - Complete validator compromise  
**Likelihood**: Medium - Common operational error  
**Mitigations**:
- ✅ Memory zeroization (zeroize crate)
- 🔄 HSM/KMS for key storage
- 🔄 Memory encryption (SGX/SEV)
- 🔄 No keys in logs/error messages

#### I2: MEV (Maximal Extractable Value) Exploitation
**Threat**: Leader extracts value by reordering transactions  
**Impact**: Medium - Unfair to users  
**Likelihood**: High - Economically incentivized  
**Mitigations**:
- 🔄 Commit-reveal for transaction ordering
- 🔄 Threshold encryption (future)
- 🔄 Fair ordering protocols
- ⚠️ **Currently vulnerable**

#### I3: Network Traffic Analysis
**Threat**: Adversary correlates IP addresses with transactions  
**Impact**: Medium - Privacy leak  
**Likelihood**: High - P2P traffic is observable  
**Mitigations**:
- 🔄 Tor/mixnet integration (future)
- 🔄 Transaction relayers
- 🔄 Dandelion++ transaction propagation

---

### D - Denial of Service

#### D1: Consensus DoS via Invalid Blocks
**Threat**: Attacker floods network with invalid blocks  
**Impact**: High - Wastes validator CPU/bandwidth  
**Likelihood**: Medium - Easy to execute  
**Mitigations**:
- ✅ Signature verification before processing
- ✅ Stake-weighted peer quotas
- ✅ Reputation-based peer scoring
- 🔄 Computational puzzles for non-validators

#### D2: State Bloat Attack
**Threat**: Attacker creates many small accounts to bloat state  
**Impact**: Medium - Increases validator hardware requirements  
**Likelihood**: Medium - Requires paying fees  
**Mitigations**:
- ✅ State rent (per-byte per epoch)
- ✅ Account minimum balance
- ✅ Garbage collection of empty accounts
- 🔄 State expiry (archive nodes only)

#### D3: Mempool Spam
**Threat**: Flood mempool with low-fee transactions  
**Impact**: Medium - Legitimate txs delayed  
**Likelihood**: High - Cheap to execute  
**Mitigations**:
- ✅ Fee prioritization
- ✅ Per-account nonce ordering
- ✅ Replace-by-fee (RBF)
- ✅ Minimum fee enforcement

#### D4: Turbine Amplification Attack
**Threat**: Attacker exploits shred retransmission for amplification  
**Impact**: Medium - Bandwidth exhaustion  
**Likelihood**: Low - Requires validator compromise  
**Mitigations**:
- ✅ Per-branch retransmit limits
- ✅ Shred signature verification
- ✅ Rate limiting per peer
- 🔄 Anomaly detection

---

### E - Elevation of Privilege

#### E1: WASM Sandbox Escape
**Threat**: Malicious contract escapes WASM sandbox  
**Impact**: Critical - Host system compromise  
**Likelihood**: Low - Wasmtime has strong isolation  
**Mitigations**:
- ✅ Wasmtime with default safety features
- ✅ No unsafe host functions exposed
- ✅ Memory limits enforced
- 🔄 Seccomp/AppArmor sandboxing at OS level

#### E2: Slashing Griefing
**Threat**: Attacker falsely triggers slashing of honest validator  
**Impact**: High - Economic loss for victim  
**Likelihood**: Low - Requires cryptographic proof  
**Mitigations**:
- ✅ Slashing proofs require BLS signatures
- ✅ Dispute resolution period
- ✅ Insurance fund for false positives
- 🔄 Multi-signature slashing approval

#### E3: Governance Capture
**Threat**: Whale accumulates >50% voting power  
**Impact**: High - Can pass malicious proposals  
**Likelihood**: Low - High capital requirement  
**Mitigations**:
- ✅ Quadratic voting (future)
- ✅ Time-locked proposals (48h)
- ✅ Veto mechanism for critical changes
- 🔄 Delegation caps per entity

---

## LINDDUN Privacy Analysis

### L - Linkability
**Threat**: Transactions from same user can be linked  
**Impact**: Medium - Privacy loss  
**Current State**: ⚠️ All transactions publicly linkable  
**Future Mitigations**:
- 🔄 Zero-knowledge proofs (zk-SNARKs)
- 🔄 Stealth addresses
- 🔄 Mixing protocols

### I - Identifiability  
**Threat**: Real-world identity tied to blockchain address  
**Impact**: High - Complete privacy loss  
**Current State**: ⚠️ IP addresses visible to peers  
**Future Mitigations**:
- 🔄 Tor integration
- 🔄 Privacy-preserving light clients

### D - Detectability
**Threat**: User's blockchain activity is observable  
**Impact**: Medium - Surveillance risk  
**Current State**: ⚠️ All transactions public  
**Future Mitigations**:
- 🔄 Private transactions (confidential assets)
- 🔄 Encrypted mempools

---

## Attack Surfaces

### 1. Validator Node
- **Network**: QUIC endpoints (9000-9100), RPC (8899), Metrics (9090)
- **Storage**: RocksDB database, snapshots
- **Memory**: Private keys, mempool, shred cache
- **Supply Chain**: Dependencies (600+ crates)

### 2. P2P Network
- **Gossipsub**: Topic-based message flooding
- **Turbine**: Tree-based shred propagation  
- **QUIC**: Connection establishment, stream multiplexing

### 3. Smart Contracts
- **WASM VM**: Bytecode execution sandbox
- **Host Functions**: Limited set of system calls
- **Gas Metering**: Prevents infinite loops

### 4. AI Mesh
- **TEE Workers**: Attestation verification
- **VCR Validation**: KZG commitment checks
- **Challenge Protocol**: Fraud proofs

---

## Mitigations Summary

### Implemented (✅)
1. Cryptographic signatures (Ed25519, BLS)
2. VRF for leader election
3. BFT consensus (HotStuff)
4. Erasure coding with packet loss tolerance
5. Gas metering and execution limits
6. State rent and fee markets
7. Peer reputation scoring
8. TEE attestation verification
9. Slashing with cryptographic proofs
10. Merkle proofs for state integrity

### In Progress (🔄)
1. KES key rotation
2. HSM/KMS integration
3. Remote signer architecture
4. Enhanced DoS protection
5. Privacy features (zk-proofs, mixing)
6. Formal verification (TLA+)
7. External security audit

### High Priority (⚠️)
1. MEV protection mechanisms
2. Network privacy (Tor/mixnet)
3. State expiry for long-term scalability

---

## Residual Risks

### Accepted Risks
1. **Quantum Computing**: Ed25519/BLS vulnerable to Shor's algorithm
   - *Mitigation Timeline*: Post-quantum migration in 5-10 years
   
2. **MEV Extraction**: Leader can reorder transactions
   - *Mitigation Timeline*: Threshold encryption in Phase 7
   
3. **Privacy Linkability**: Transaction graphs are public
   - *Mitigation Timeline*: zk-SNARK integration in Phase 7

### Monitoring Required
1. Peer-to-peer network health
2. Validator key compromise indicators  
3. Unusual slashing patterns
4. State growth rate

---

## Security Testing

### Unit Tests
- ✅ 165+ unit tests covering cryptographic primitives
- ✅ Signature verification edge cases
- ✅ Merkle proof validation
- ✅ Gas metering limits

### Property Tests
- 🔄 Consensus safety (no conflicting finalization)
- 🔄 Consensus liveness (eventual finality)
- 🔄 Account balance conservation
- 🔄 State transition determinism

### Fuzzing Targets
- 🔄 Transaction deserialization
- 🔄 Block parsing
- 🔄 WASM bytecode loading
- 🔄 P2P message handling

---

## Audit Recommendations

### Critical Components for External Audit
1. **Consensus Protocol**: HotStuff safety/liveness proofs
2. **Cryptography**: Key generation, signature schemes, VRF
3. **WASM Runtime**: Sandbox isolation, gas metering
4. **TEE Integration**: Attestation verification, VCR validation
5. **Economic Mechanisms**: Slashing, staking, fee markets

### Audit Scope
- **Code Review**: 23,000+ lines of Rust
- **Formal Verification**: TLA+ specifications
- **Penetration Testing**: Network, RPC endpoints, smart contracts
- **Economic Analysis**: Game theory, incentive compatibility

---

**Last Updated**: October 13, 2025  
**Next Review**: Before mainnet launch (Phase 7)

