# Aether Remote Signer Architecture

> Scope checked 2026-10-18: this is threat-model/design guidance, not an audit
> certificate or a list of fully integrated services. Current executable wiring
> is in [architecture](../architecture.md); executed checks must be reported
> separately. The maintained test suite is [E2E-only](../testing.md). Historical
> unit/property/vector-test references below are requirements or prior evidence,
> not current runnable test targets. AI workers and remote signers require their
> own implementation/deployment evidence.

**Version**: 1.0  
**Date**: October 13, 2025  
**Purpose**: Secure validator key management with HSM/KMS integration

## Overview

The Remote Signer architecture separates validator private keys from the block production node, storing them securely in Hardware Security Modules (HSM) or Key Management Services (KMS). This reduces the attack surface by ensuring private keys never exist in node memory.

### Architecture

```
┌─────────────────────┐         ┌──────────────────────┐
│  Validator Node     │         │   Remote Signer      │
│                     │         │                      │
│  ┌───────────────┐  │         │  ┌────────────────┐  │
│  │ Consensus     │  │         │  │  Signing Logic │  │
│  │ Engine        │──┼────────>├──│                │  │
│  └───────────────┘  │ gRPC/   │  │  - Ed25519     │  │
│                     │ TLS     │  │  - BLS         │  │
│  ┌───────────────┐  │         │  │  - KES         │  │
│  │ Block         │  │         │  └────────┬───────┘  │
│  │ Proposer      │──┼─────┐   │           │          │
│  └───────────────┘  │     │   │  ┌────────▼───────┐  │
│                     │     └──>│  │   Rate Limiter │  │
│  ┌───────────────┐  │         │  └────────┬───────┘  │
│  │ Public Keys   │  │         │           │          │
│  │ Only          │  │         │  ┌────────▼───────┐  │
│  └───────────────┘  │         │  │  HSM / KMS     │  │
└─────────────────────┘         │  │                │  │
                                │  │  - AWS KMS     │  │
                                │  │  - YubiHSM     │  │
                                │  │  - Azure KeyV. │  │
                                │  └────────────────┘  │
                                └──────────────────────┘
```

## Key Components

### 1. Validator Node
- **Role**: Block production, consensus participation
- **Keys**: Only public keys (verification keys)
- **Security**: No private keys in memory
- **Communication**: gRPC + mTLS to remote signer

### 2. Remote Signer
- **Role**: Sign consensus messages, blocks, votes
- **Keys**: Private keys stored in HSM/KMS
- **Security**: Hardware-backed key storage, rate limiting
- **Communication**: Authenticated gRPC endpoints

### 3. HSM/KMS Integration
- **AWS KMS**: Cloud-based key management
- **YubiHSM 2**: USB hardware security module
- **Azure Key Vault**: Microsoft cloud KMS
- **Google Cloud KMS**: Google cloud option

## Protocol

### Message Signing Flow

1. **Validator** needs to sign a message (block, vote, etc.)
2. **Validator** sends signing request to **Remote Signer** via gRPC
3. **Remote Signer** validates request:
   - Check validator identity (mTLS)
   - Rate limit check (prevent spam)
   - Message format validation
   - Replay protection (nonce/timestamp)
4. **Remote Signer** requests signature from **HSM/KMS**
5. **HSM/KMS** generates signature using stored private key
6. **Remote Signer** returns signature to **Validator**
7. **Validator** attaches signature to message and broadcasts

### Security Properties

- ✅ **Key Isolation**: Private keys never leave HSM
- ✅ **Replay Protection**: Nonces prevent message replay
- ✅ **Rate Limiting**: Prevents DoS on signer
- ✅ **Audit Logging**: All signature requests logged
- ✅ **Mutual TLS**: Both sides authenticated
- ✅ **Slashing Protection**: Double-sign detection

## Implementation

### gRPC Service Definition

```protobuf
service RemoteSigner {
  // Sign a block proposal
  rpc SignBlock(SignBlockRequest) returns (SignBlockResponse);
  
  // Sign a consensus vote
  rpc SignVote(SignVoteRequest) returns (SignVoteResponse);
  
  // Get public verification key
  rpc GetPublicKey(GetPublicKeyRequest) returns (GetPublicKeyResponse);
  
  // Health check
  rpc Health(HealthRequest) returns (HealthResponse);
}

message SignBlockRequest {
  bytes block_hash = 1;
  uint64 slot = 2;
  bytes block_data = 3;
  uint64 nonce = 4;
}

message SignBlockResponse {
  bytes signature = 1;
  SignatureType type = 2;  // Ed25519, BLS, KES
}
```

### Configuration

```toml
[remote_signer]
enabled = true
endpoint = "https://signer.validator.local:9443"
timeout_ms = 1000

[remote_signer.tls]
client_cert = "/etc/aether/validator-client.crt"
client_key = "/etc/aether/validator-client.key"
ca_cert = "/etc/aether/signer-ca.crt"

[remote_signer.hsm]
provider = "aws_kms"  # or "yubihsm", "azure_kv", "google_kms"
key_id = "arn:aws:kms:us-east-1:123456789:key/..."
region = "us-east-1"

[remote_signer.rate_limiting]
max_signatures_per_minute = 120  # 2 signatures/second
max_signatures_per_hour = 5000

[remote_signer.slashing_protection]
enabled = true
database = "/var/lib/aether/slashing.db"
```

## HSM Integration

### AWS KMS

```rust
use aws_sdk_kms::Client;

async fn sign_with_kms(
    client: &Client,
    key_id: &str,
    message: &[u8],
) -> Result<Vec<u8>> {
    let response = client
        .sign()
        .key_id(key_id)
        .message(Blob::new(message))
        .signing_algorithm(SigningAlgorithmSpec::EcdsaSha256)
        .send()
        .await?;
    
    Ok(response.signature.unwrap().into_inner())
}
```

### YubiHSM 2

```rust
use yubihsm::{Client, Connector};

fn sign_with_yubihsm(
    client: &mut Client,
    key_id: u16,
    message: &[u8],
) -> Result<Vec<u8>> {
    let signature = client.sign_ecdsa_sha256(key_id, message)?;
    Ok(signature.to_vec())
}
```

## Slashing Protection

The remote signer MUST implement slashing protection to prevent:
- **Double-signing**: Signing conflicting blocks at same height
- **Surround voting**: Signing votes that surround previous votes

### Protection Database

```sql
CREATE TABLE signed_blocks (
    validator_id BLOB NOT NULL,
    slot INTEGER NOT NULL,
    block_hash BLOB NOT NULL,
    signature BLOB NOT NULL,
    timestamp INTEGER NOT NULL,
    PRIMARY KEY (validator_id, slot)
);

CREATE TABLE signed_votes (
    validator_id BLOB NOT NULL,
    slot INTEGER NOT NULL,
    block_hash BLOB NOT NULL,
    signature BLOB NOT NULL,
    timestamp INTEGER NOT NULL,
    PRIMARY KEY (validator_id, slot, block_hash)
);
```

### Double-Sign Check

```rust
fn check_double_sign(
    db: &Database,
    validator_id: &[u8],
    slot: u64,
    block_hash: &[u8],
) -> Result<()> {
    // Check if we've already signed for this slot
    if let Some(existing) = db.get_signed_block(validator_id, slot)? {
        if existing.block_hash != block_hash {
            // DOUBLE SIGN DETECTED!
            return Err(SignerError::DoubleSign {
                slot,
                existing_hash: existing.block_hash,
                new_hash: block_hash.to_vec(),
            });
        }
    }
    Ok(())
}
```

## Deployment

### High Availability

For production validators, run multiple remote signers:

```
┌─────────────┐
│ Validator   │
└──────┬──────┘
       │
   ┌───┴────┐
   │ LB/HA  │
   └───┬────┘
       │
  ┌────┼────┐
  │    │    │
┌─▼──┐ │  ┌─▼──┐
│S1  │ │  │S2  │
└─┬──┘ │  └─┬──┘
  │    │    │
┌─▼────▼────▼─┐
│   HSM Cluster│
└─────────────┘
```

### Disaster Recovery

1. **Key Backup**: HSM keys backed up securely offline
2. **Geo-redundancy**: Signers in multiple regions
3. **Failover**: Automatic failover to backup signer
4. **Recovery Time**: < 1 minute RTO for primary failure

## Monitoring

### Metrics

```
aether_signer_requests_total
aether_signer_requests_duration_ms
aether_signer_errors_total{type="rate_limit|invalid_request|hsm_error"}
aether_signer_double_sign_attempts_total
aether_signer_hsm_latency_ms
```

### Alerts

- **High Error Rate**: > 1% of requests failing
- **HSM Latency**: > 100ms p99 latency
- **Double Sign Attempt**: ANY attempt triggers critical alert
- **Rate Limit Hit**: Sustained rate limiting indicates attack
- **Cert Expiry**: TLS certificates expiring in < 30 days

## Best Practices

1. **Never expose HSM directly**: Always use remote signer as intermediary
2. **Use hardware-backed keys**: No software-only key storage in production
3. **Enable slashing protection**: Critical for preventing validator slashing
4. **Monitor everything**: All signing requests must be logged and monitored
5. **Test failover regularly**: Practice DR scenarios monthly
6. **Rotate credentials**: TLS certificates rotated every 90 days
7. **Limit network access**: Signer only accessible from validator IP
8. **Use separate HSMs per validator**: Don't share HSMs across validators

## Migration from Local Keys

### Step 1: Generate keys in HSM
```bash
aws kms create-key --key-usage SIGN_VERIFY \
  --key-spec ECC_NIST_P256 \
  --description "Aether Validator Key"
```

### Step 2: Export public key
```bash
aws kms get-public-key --key-id $KEY_ID > validator_pubkey.der
```

### Step 3: Deploy remote signer
```bash
docker run -d \
  -p 9443:9443 \
  -v /etc/aether/certs:/certs \
  -e AWS_KMS_KEY_ID=$KEY_ID \
  aether/remote-signer:latest
```

### Step 4: Update validator config
```toml
[signing]
mode = "remote"
remote_signer_url = "https://signer.validator.local:9443"
```

### Step 5: Restart validator
```bash
systemctl restart aether-validator
```

### Step 6: Verify operation
```bash
# Check logs for successful remote signing
journalctl -u aether-validator | grep "remote_signer"

# Check signer metrics
curl https://signer.validator.local:9090/metrics
```

---

**Status**: Production-ready architecture  
**Next Steps**: Implement gRPC service and HSM integrations  
**Security Review**: Required before mainnet deployment

