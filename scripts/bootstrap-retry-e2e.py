#!/usr/bin/env python3
"""Real validator journey: a late bootstrap seed and a seed restart recover."""

import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("AETHER_BIN_DIR", ROOT / "target/release")).resolve()


def main():
    def terminate(_signal, _frame):
        raise SystemExit(143)

    signal.signal(signal.SIGTERM, terminate)
    (ROOT / "data").mkdir(exist_ok=True)
    data = Path(tempfile.mkdtemp(prefix="bootstrap-e2e-", dir=ROOT / "data"))
    processes = {}
    handles = []
    reservations = []
    failed = True

    def port():
        reservation = socket.socket()
        reservation.bind(("127.0.0.1", 0))
        reservations.append(reservation)
        return reservation.getsockname()[1], reservation

    ports = {i: [port() for _ in range(3)] for i in range(1, 5)}

    def rpc(i, method, params=None):
        request = urllib.request.Request(
            f"http://127.0.0.1:{ports[i][0][0]}",
            data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method,
                             "params": params or []}).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=2) as response:
            envelope = json.load(response)
        if "error" in envelope:
            raise RuntimeError(envelope["error"])
        return envelope["result"]

    def start(i):
        for _, reservation in ports[i]:
            reservation.close()
        env = os.environ.copy()
        # A star with no reverse dial to validator 3: its connection can only
        # recover through its own configured-bootstrap retries.
        env.update({
            "AETHER_GENESIS_PATH": str(data / "genesis/genesis.json"),
            "AETHER_VALIDATOR_KEY": str(data / f"genesis/validator-{i}.key"),
            "AETHER_NODE_DB_PATH": str(data / f"node{i}"),
            "AETHER_RPC_BIND_IP": "127.0.0.1",
            "AETHER_RPC_PORT": str(ports[i][0][0]),
            "AETHER_P2P_PORT": str(ports[i][1][0]),
            "AETHER_METRICS_PORT": str(ports[i][2][0]),
            "AETHER_BOOTSTRAP_PEERS": "" if i == 1 else
                f"/dns4/localhost/tcp/{ports[1][1][0]}",
            "RUST_LOG": "info",
        })
        log = (data / f"node{i}.log").open("ab")
        handles.append(log)
        processes[i] = subprocess.Popen([str(BIN / "aether-node")], env=env,
                                        stdout=log, stderr=subprocess.STDOUT)

    def stop(i):
        process = processes.pop(i)
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

    def wait_for(predicate, description, timeout=90):
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            for i, process in processes.items():
                if process.poll() is not None:
                    raise RuntimeError(f"validator {i} exited: see {data}/node{i}.log")
            try:
                if predicate():
                    return
            except (OSError, ValueError, RuntimeError) as error:
                last_error = error
            time.sleep(0.25)
        raise RuntimeError(f"timed out waiting for {description}: {last_error}; logs: {data}")

    def info(i):
        return rpc(i, "aeth_getNodeInfo")

    def connected_and_finalized():
        return all(info(i)["peers"] > 0 and info(i)["finalizedSlot"] > 0
                   for i in range(1, 5))

    try:
        subprocess.run([str(BIN / "genesis-ceremony"), "--validators", "4",
                        "--output-dir", str(data / "genesis"),
                        "--genesis-delay-ms", "0"], check=True,
                       stdout=subprocess.DEVNULL, timeout=30)
        start(3)
        wait_for(lambda: info(3)["peers"] == 0, "isolated follower RPC", timeout=15)
        time.sleep(6)  # Its only seed is bound but not listening: dial must fail.
        assert info(3)["peers"] == 0 and info(3)["finalizedSlot"] == 0
        print("Follower is isolated before its bootstrap seed starts.", flush=True)
        for i in (1, 2, 4):
            start(i)
        wait_for(connected_and_finalized, "late-seed recovery and four-validator finality")
        print("Late bootstrap seed recovered; all four validators finalized.", flush=True)

        stop(1)
        wait_for(lambda: info(3)["peers"] == 0, "seed disconnect", timeout=15)
        before = info(3)["finalizedSlot"]
        # Slots are 500ms. Keep the star disconnected for more than one
        # 64-slot sync page, so recovery must cross a page without quorum blocks.
        time.sleep(36)
        start(1)
        wait_for(lambda: connected_and_finalized() and info(3)["finalizedSlot"] > before,
                 "seed restart, follower reconnect, and resumed finality")

        recipient = "0x" + "7b" * 20
        before_balance = rpc(3, "aeth_getAccount", [recipient])
        before_balance = int(before_balance["balance"]) if before_balance else 0
        subprocess.run([str(BIN / "aetherctl"), "chain", "--rpc",
                        f"http://127.0.0.1:{ports[1][0][0]}", "transfer", "--dev", "0",
                        "--to", recipient, "--amount", "314159", "--wait"],
                       check=True, timeout=90, stdout=subprocess.DEVNULL)
        wait_for(lambda: int((rpc(3, "aeth_getAccount", [recipient]) or {})
                             .get("balance", 0)) == before_balance + 314159,
                 "exact finalized recipient balance on reconnected follower")
        failed = False
        print("Bootstrap retry E2E passed: late seed, disconnect/restart, finality, "
              "and exact cross-validator transfer balance.", flush=True)
    finally:
        for i in list(processes):
            stop(i)
        for handle in handles:
            handle.close()
        for reservation in reservations:
            reservation.close()
        if failed or os.environ.get("KEEP_DEVNET_DATA"):
            print(f"Bootstrap journey logs: {data}", flush=True)
        else:
            shutil.rmtree(data)


if __name__ == "__main__":
    main()
