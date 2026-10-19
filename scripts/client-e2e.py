#!/usr/bin/env python3
"""Live Python SDK transfer interoperability journey across two validators."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "sdks/python/src"))

from aether_sdk import AetherClient, Keypair


def balance(account: Optional[dict]) -> int:
    value = 0 if account is None else account.get("balance", 0)
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, str) and value.isdecimal():
        return int(value)
    raise ValueError(f"account returned an unsupported balance representation: {value!r}")


def nonce(account: Optional[dict]) -> int:
    value = 0 if account is None else account.get("nonce", 0)
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, str) and value.isdecimal():
        return int(value)
    raise ValueError(f"pending account returned an unsupported nonce: {value!r}")


rpc_from = os.environ.get("AETHER_RPC_FROM", "http://127.0.0.1:8545")
rpc_read = os.environ.get("AETHER_RPC_READ", "http://127.0.0.1:8547")
ctl = os.environ.get("AETHERCTL_BIN", "target/release/aetherctl")
funding_amount = int(os.environ.get("AETHER_E2E_FUNDING", "1000000000"))
transfer_amount = int(os.environ.get("AETHER_E2E_AMOUNT", "12345"))
if transfer_amount <= 0 or funding_amount <= transfer_amount:
    raise ValueError("funding amount must exceed the positive transfer amount")

sender = Keypair.generate()
recipient = Keypair.generate()
print(f"Funding temporary Python account {sender.address_hex} through the Rust CLI")
subprocess.run(
    [
        ctl,
        "chain",
        "--rpc",
        rpc_from,
        "transfer",
        "--dev",
        "0",
        "--to",
        sender.address_hex,
        "--amount",
        str(funding_amount),
        "--wait",
    ],
    check=True,
)

from_client = AetherClient(rpc_from)
read_client = AetherClient(rpc_read)
before_balance = balance(read_client.get_account(recipient.address_hex))
chain_id = from_client.get_chain_id()
fee_parameters = from_client.get_fee_parameters()
if fee_parameters.chain_id != chain_id:
    raise ValueError("fee parameters chain id does not match RPC chain id")
pending = from_client.get_pending_account(sender.address_hex)
tx = (
    from_client.transfer()
    .to(recipient.address_hex)
    .amount(transfer_amount)
    .memo("python rust interoperability e2e")
    .build(
        keypair=sender,
        chain_id=chain_id,
        nonce=nonce(pending),
        fee_parameters=fee_parameters,
    )
)
submitted = from_client.submit(tx)
receipt = from_client.wait_for_finality(
    submitted.tx_hash,
    timeout_seconds=float(os.environ.get("AETHER_E2E_TIMEOUT_SECONDS", "90")),
)
status = from_client.get_transaction_status(submitted.tx_hash)
if status.get("status") != "finalized" or status.get("success") is not True:
    raise RuntimeError(f"transaction did not finalize successfully: {status!r}")

after_balance = balance(read_client.get_account(recipient.address_hex))
if after_balance - before_balance != transfer_amount:
    raise AssertionError(
        f"cross-validator balance mismatch: expected delta {transfer_amount}, "
        f"got {after_balance - before_balance}"
    )

print(
    json.dumps(
        {
            "result": "PASS",
            "sender": sender.address_hex,
            "recipient": recipient.address_hex,
            "chain_id": chain_id,
            "transaction_hash": submitted.tx_hash,
            "receipt_slot": receipt.slot,
            "sender_rpc": rpc_from,
            "balance_read_rpc": rpc_read,
            "recipient_balance_before": before_balance,
            "recipient_balance_after": after_balance,
            "transfer_amount": transfer_amount,
        },
        indent=2,
    )
)
