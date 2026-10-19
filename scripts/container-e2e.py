#!/usr/bin/env python3
"""Exercise the Compose validators through their real JSON-RPC endpoints."""

import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


RPCS = [os.environ.get(f"RPC_URL_{i}", f"http://validator-{i}:8545") for i in range(1, 5)]


def request(url, method="GET", payload=None, timeout=3):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"} if data is not None else {},
        method=method,
    )
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return response.status, response.read()


def wait_ready():
    deadline = time.monotonic() + 120
    pending = set(RPCS)
    while pending and time.monotonic() < deadline:
        for rpc in list(pending):
            try:
                status, _ = request(f"{rpc}/ready")
                if status == 200:
                    pending.remove(rpc)
            except (OSError, urllib.error.URLError):
                pass
        if pending:
            time.sleep(1)
    if pending:
        raise RuntimeError(f"validators did not become ready: {', '.join(sorted(pending))}")


def run_cli(*args, timeout=90):
    result = subprocess.run(
        ["aetherctl", *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return result.stdout


def main():
    print("Waiting for all validators to finalize over the Compose network...", flush=True)
    wait_ready()

    # Confirm service DNS and P2P connectivity from validator RPC observations.
    for index, rpc in enumerate(RPCS, start=1):
        _, body = request(
            rpc,
            method="POST",
            payload={"jsonrpc": "2.0", "method": "aeth_getNodeInfo", "params": [], "id": index},
        )
        info = json.loads(body)["result"]
        if index > 1 and int(info.get("peerCount", 0)) < 1:
            raise RuntimeError(f"validator {index} has no P2P peers: {info}")
        print(
            f"validator {index}: finalized={info.get('finalizedSlot')} peers={info.get('peerCount')}",
            flush=True,
        )

    with tempfile.TemporaryDirectory(prefix="aether-container-e2e-") as tmp:
        key_path = os.path.join(tmp, "recipient.json")
        key_summary = json.loads(run_cli("keys", "generate", "--out", key_path))
        recipient = key_summary["address"]

        print("Submitting a signed transfer through validator 1...", flush=True)
        run_cli(
            "chain",
            "--rpc",
            RPCS[0],
            "transfer",
            "--dev",
            "0",
            "--to",
            recipient,
            "--amount",
            "5000000000",
            "--wait",
            timeout=120,
        )

        output = run_cli("chain", "--rpc", RPCS[3], "account", recipient)
        account = json.loads(output)
        balance = account["finalized"]["balance"]
        if int(balance) != 5_000_000_000:
            raise RuntimeError(f"validator 4 reports unexpected finalized balance: {balance}")

    print("Container E2E passed: P2P peers, signed transfer, finality, and cross-validator state.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001 - print a concise CI failure reason
        print(f"Container E2E failed: {exc}", file=sys.stderr)
        sys.exit(1)
