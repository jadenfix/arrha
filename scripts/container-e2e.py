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
    last_readiness = {}
    while pending and time.monotonic() < deadline:
        for rpc in list(pending):
            try:
                status, body = request(f"{rpc}/ready")
            except urllib.error.HTTPError as exc:
                status, body = exc.code, exc.read()
            except (OSError, urllib.error.URLError) as exc:
                last_readiness[rpc] = {"error": str(exc)}
                continue
            try:
                details = json.loads(body)
            except (TypeError, ValueError):
                details = {"body": body.decode("utf-8", errors="replace")}
            if not isinstance(details, dict):
                details = {"body": details}
            last_readiness[rpc] = {"http_status": status, **details}
            if status == 200:
                pending.remove(rpc)
        if pending:
            time.sleep(1)
    if pending:
        for index, rpc in enumerate(RPCS, start=1):
            if rpc not in pending:
                continue
            payload = {
                "jsonrpc": "2.0",
                "method": "aeth_getNodeInfo",
                "params": [],
                "id": index,
            }
            try:
                _, body = request(rpc, method="POST", payload=payload)
                response = json.loads(body)
                last_readiness.setdefault(rpc, {})["node_info"] = response.get("result", response)
            except Exception as exc:  # include RPC reachability in the timeout diagnostic
                last_readiness.setdefault(rpc, {})["node_info_error"] = str(exc)
        raise RuntimeError(
            "validators did not become ready: "
            f"{json.dumps({rpc: last_readiness.get(rpc, {}) for rpc in sorted(pending)}, sort_keys=True)}"
        )


def run_cli(*args, timeout=90):
    result = subprocess.run(
        ["aetherctl", *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return result.stdout


def get_node_info(index, rpc):
    payload = {
        "jsonrpc": "2.0",
        "method": "aeth_getNodeInfo",
        "params": [],
        "id": index,
    }
    _, body = request(rpc, method="POST", payload=payload)
    response = json.loads(body)
    if not isinstance(response.get("result"), dict):
        raise RuntimeError(f"validator {index} returned invalid node info: {response}")
    return response["result"]


def wait_full_mesh():
    deadline = time.monotonic() + 30
    pending = set(enumerate(RPCS, start=1))
    last_node_info = {}
    while pending and time.monotonic() < deadline:
        for index, rpc in list(pending):
            try:
                info = get_node_info(index, rpc)
                last_node_info[rpc] = info
                peers = info.get("peers")
                if isinstance(peers, int) and peers >= 3:
                    pending.remove((index, rpc))
            except (OSError, ValueError, RuntimeError, urllib.error.URLError) as exc:
                last_node_info[rpc] = {"error": str(exc)}
        if pending:
            time.sleep(1)
    if pending:
        details = {
            rpc: last_node_info.get(rpc, {})
            for _, rpc in sorted(pending)
        }
        raise RuntimeError(
            "validators did not establish the full mesh (3 peers each): "
            f"{json.dumps(details, sort_keys=True)}"
        )
    for index, rpc in enumerate(RPCS, start=1):
        info = last_node_info[rpc]
        print(
            f"validator {index}: finalized={info.get('finalizedSlot')} peers={info['peers']}",
            flush=True,
        )


def main():
    print("Waiting for all validators to finalize over the Compose network...", flush=True)
    wait_ready()

    # Confirm service DNS and P2P connectivity from validator RPC observations.
    wait_full_mesh()

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
