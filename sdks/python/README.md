# Aether Python SDK

Synchronous JSON-RPC helpers, Ed25519-signed transfer support, and AI job request
builders for Arrha / Aether. The package requires Python 3.9+ and PyCA
`cryptography` for Ed25519 signing.

## Install from this repository

From the repository root:

```bash
python3 -m venv work/python-sdk-venv
work/python-sdk-venv/bin/python -m pip install --upgrade pip setuptools wheel
work/python-sdk-venv/bin/python -m pip install ./sdks/python
work/python-sdk-venv/bin/python -c 'import aether_sdk; print(aether_sdk.__file__)'
```

The isolated environment matters on systems that ship older pip/setuptools; the
regular install above builds and installs the package wheel.

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

`client.request_airdrop(address, amount)` returns an `AirdropResponse` acknowledgement,
not a transaction hash. The local faucet directly seeds state only when the
single-validator node enables it; multi-validator devnets disable this method.
Use the CLI transfer path to fund an account on the four-validator devnet.

## Signed local/devnet transfer

```python
from aether_sdk import AetherClient, Keypair

client = AetherClient("http://127.0.0.1:8545")
keypair = Keypair.generate()  # fund this address on the local devnet first
chain_id = client.get_chain_id()
pending = client.get_pending_account(keypair.address_hex)
nonce = 0 if pending is None else int(pending["nonce"])
fees = client.get_fee_parameters()

tx = (
    client.transfer()
    .to("0x" + "8b" * 20)
    .amount(1_000_000)
    .memo("example")
    .build(keypair=keypair, chain_id=chain_id, nonce=nonce, fee_parameters=fees)
)

submitted = client.submit(tx)  # raw bincode transaction; returned hash is checked
receipt = client.wait_for_finality(submitted.tx_hash)
print(receipt)
```

The SDK uses the node's live chain ID, pending nonce, and fee parameters; it signs the
same canonical bincode hash as Rust and submits the full transaction through
`aeth_sendRawTransaction`. The example is for local/devnet use. Fund the generated
address before submitting. The retained cross-validator journey is
`python3 scripts/client-e2e.py`; it funds a temporary wallet with the local CLI,
submits through this SDK, waits for a successful finalized receipt, and checks the
recipient balance through a second validator.

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
