# Aether Python SDK

Lightweight client utilities for submitting transactions and AI jobs to the Aether blockchain.

## Payload-building example

```python
from aether_sdk import AetherClient

client = AetherClient("http://127.0.0.1:8545")
tx = (
    client.transfer()
    .to("0x8b0b54d2248a3a5617b6bd8a2fd4cc8ebc0f2e90")
    .amount(1_000_000)
    .memo("phase7")
    .build(
        sender="0x1111111111111111111111111111111111111111",
        sender_public_key="0x" + "a1" * 32,
        signature="0x" + "b2" * 32,
        nonce=7,
    )
)

print(tx)  # illustrative payload; keys/signature above are placeholders
```

This example does not sign a valid transaction or prove Python RPC submission
against the current node. Use the root README CLI/Rust chain journey for an
executed signed transfer path. Python unit tests were removed under the
repository E2E-only policy.
