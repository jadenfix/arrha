#!/usr/bin/env python3
"""Verify finalized RPC ingestion survives an indexer process restart."""

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


RPC_URL = os.environ.get("AETHER_RPC_URL", "http://127.0.0.1:8545").rstrip("/")
INDEXER_BIN = os.environ.get("AETHER_INDEXER_BIN", "target/release/aether-indexer")
WAIT_SECONDS = int(os.environ.get("INDEXER_E2E_TIMEOUT_SECS", "180"))


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def get_json(url, timeout=3):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as exc:
        return exc.code, json.loads(exc.read())


def get_node_info():
    payload = json.dumps(
        {"jsonrpc": "2.0", "method": "aeth_getNodeInfo", "params": [], "id": 1}
    ).encode()
    request = urllib.request.Request(
        f"{RPC_URL}/",
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=3) as response:
        return response.status, json.loads(response.read())


def wait_caught_up(process, api, log_path):
    deadline = time.monotonic() + WAIT_SECONDS
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"indexer exited with {process.returncode}: {tail(log_path)}"
            )
        try:
            code, status = get_json(f"{api}/status")
            if (
                code == 200
                and status.get("status") == "caught_up"
                and status.get("caught_up") is True
            ):
                return status
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.5)
    raise RuntimeError(f"indexer did not catch up within {WAIT_SECONDS}s: {tail(log_path)}")


def tail(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as log:
            return "".join(log.readlines()[-40:])
    except OSError:
        return "(no indexer log available)"


def start_indexer(database, port, log_path):
    env = os.environ.copy()
    env.update(
        {
            "AETHER_RPC_URL": RPC_URL,
            "INDEXER_DB_PATH": database,
            "INDEXER_BIND_IP": "127.0.0.1",
            "INDEXER_PORT": str(port),
            "INDEXER_POLL_INTERVAL_MS": "500",
            "INDEXER_RETRY_INTERVAL_MS": "500",
        }
    )
    log = open(log_path, "a", encoding="utf-8")
    process = subprocess.Popen([INDEXER_BIN], env=env, stdout=log, stderr=subprocess.STDOUT)
    return process, log


def stop_indexer(process, log):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=10)
    log.close()


def main():
    node_code, node_response = get_node_info()
    node_info = node_response.get("result", {})
    if node_code != 200 or not isinstance(node_info, dict):
        raise RuntimeError(f"node RPC is unavailable at {RPC_URL}")
    if int(node_info.get("finalizedSlot", 0)) <= 0:
        raise RuntimeError("indexer restart journey requires a node with finalized blocks")

    port = free_port()
    api = f"http://127.0.0.1:{port}"
    with tempfile.TemporaryDirectory(prefix="aether-indexer-restart-") as tmp:
        database = os.path.join(tmp, "index")
        log_path = os.path.join(tmp, "indexer.log")
        first, first_log = start_indexer(database, port, log_path)
        try:
            first_status = wait_caught_up(first, api, log_path)
            slot = first_status.get("indexed_slot")
            if slot is None or int(first_status.get("blocks_indexed", 0)) <= 0:
                raise RuntimeError(f"indexer caught up without persisting blocks: {first_status}")
            _, checkpoint = get_json(f"{api}/block/{slot}")
            if checkpoint.get("slot") != slot:
                raise RuntimeError(f"indexer did not serve its persisted checkpoint: {checkpoint}")
            identity = (
                first_status.get("chain_id"),
                first_status.get("genesis_hash"),
                first_status.get("chain_identity"),
            )
            if not all(identity):
                raise RuntimeError(f"indexer did not report complete chain identity: {identity}")
        finally:
            stop_indexer(first, first_log)

        second, second_log = start_indexer(database, port, log_path)
        try:
            second_status = wait_caught_up(second, api, log_path)
            if (
                second_status.get("chain_id"),
                second_status.get("genesis_hash"),
                second_status.get("chain_identity"),
            ) != identity:
                raise RuntimeError("chain identity changed across indexer restart")
            if int(second_status.get("blocks_indexed", 0)) < int(
                first_status["blocks_indexed"]
            ):
                raise RuntimeError("persisted block count decreased across indexer restart")
            _, restored = get_json(f"{api}/block/{slot}")
            if restored != checkpoint:
                raise RuntimeError(f"persisted checkpoint changed across restart: {restored}")
        finally:
            stop_indexer(second, second_log)

    print(
        "Indexer restart E2E passed: finalized RPC ingestion, chain identity, "
        f"and persisted slot {slot} survived restart."
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001 - print an actionable failure
        print(f"Indexer restart E2E failed: {exc}", file=sys.stderr)
        sys.exit(1)
