# Aether Python SDK

Synchronous JSON-RPC helpers, transfer payload builders, and AI job request builders
for Arrha / Aether. The package uses Python's standard library and requires Python
3.9+ according to `pyproject.toml`.

## Install from this repository

From the repository root:

```bash
python3 -m venv .venv
. .venv/bin/activate
python3 -m pip install -e sdks/python
```

No hosted endpoint, signing key, or node process is bundled. Start a local validator
network using the [root guide](../../GETTING_STARTED.md).

## Read a local node

With a node listening on port 8545:

```python
from aether_sdk import AetherClient

client = AetherClient("http://127.0.0.1:8545")
print(client.get_slot_number())
print(client.get_finalized_slot())
print(client.get_health())
```

Reads are synchronous HTTP requests with a 10-second timeout. RPC failures raise
`ValueError`; connection failures raise `ConnectionError`. A slot number is not a
transaction finality check.

## Payload-building example

```python
from aether_sdk import AetherClient

client = AetherClient("http://127.0.0.1:8545")
tx = (
    client.transfer()
    .to("0x" + "8b" * 20)
    .amount(1_000_000)
    .memo("example")
    .build(
        sender="0x" + "11" * 20,
        sender_public_key="0x" + "a1" * 32,
        signature="0x" + "b2" * 64,
        nonce=7,
    )
)

print(tx.to_rpc_transaction())  # structural example; these are placeholder credentials
```

This example only constructs a payload: `TransferBuilder` takes a caller-supplied
public key and 64-byte signature and does not sign. `Transaction.hash()` computes a
Python JSON hash, so do not assume it is the node's signed transaction identifier.
`client.submit(tx)` calls `aeth_sendTransaction`; acceptance is distinct from
finalization. The placeholder example above must not be submitted as a valid transfer.

Use the [CLI/Rust chain journey](../../README.md#accounts-transfers-and-a-wasm-contract)
for the repository's signed transfer/deploy/call path. Python submission is not covered
by the retained process E2E journey.

## Job request builders

`client.job()` constructs a `JobRequest`; `to_submission()` and
`client.prepare_job_submission(job)` return a POST description for `/v1/jobs`.
They do not send HTTP requests. The node exposes JSON-RPC and `/health`, not an
integrated `/v1/jobs` service, and these builders do not start inference, verification,
or escrow settlement.

## Validation

Python unit-test runners were removed under the repository's E2E-only policy.
See [testing](../../docs/testing.md) for retained journeys and their boundaries.
The Python examples here are usage documentation, not evidence of live Python
settlement or production compatibility.
