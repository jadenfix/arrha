#!/usr/bin/env python3
"""Compact a recorded devnet session (the `liveness` benchmark scenario) into
one JSON file for visualisation.

Input:  <results>/liveness-events/node*.events.jsonl  (AETHER_EVENT_LOG of each validator)
        <results>/liveness-events/genesis.json
        <results>/liveness-schedule.json               (load start, kill, restart times)
Output: JSON with validators, blocks (proposer, parent, txs, receive/vote/QC/final
        times per validator) and validator up/down intervals. Times are ms relative
        to the load start. Nothing is synthesised: every number comes from the logs.

usage: export_session.py <results-dir> <out.json> [--from-s -5] [--to-s 105]
"""
import glob
import hashlib
import json
import os
import sys


def main():
    res, out = sys.argv[1], sys.argv[2]
    args = sys.argv[3:]
    lo = float(args[args.index("--from-s") + 1]) if "--from-s" in args else -5.0
    hi = float(args[args.index("--to-s") + 1]) if "--to-s" in args else 105.0
    sched = json.load(open(os.path.join(res, "liveness-schedule.json")))
    genesis = json.load(open(os.path.join(res, "liveness-events", "genesis.json")))
    t0 = sched["startMs"]

    validators = []
    for i, v in enumerate(genesis["validators"]):
        addr = hashlib.sha256(bytes(v["pubkey"])).digest()[:20]
        validators.append({"id": i + 1, "short": addr[:4].hex(), "address": "0x" + addr.hex()})
    by_short = {v["short"]: v["id"] for v in validators}

    events = []
    for f in sorted(glob.glob(os.path.join(res, "liveness-events", "node*.events.jsonl"))):
        for line in open(f):
            line = line.strip()
            if line:
                events.append(json.loads(line))
    events.sort(key=lambda e: e["t"])

    def rel(t):
        return t - t0

    blocks = {}

    def blk(h):
        return blocks.setdefault(h, {"hash": h, "recv": {}, "votes": {}, "qc": {}, "final": {}})

    for e in events:
        n = by_short.get(e["node"])
        if n is None or not (lo * 1000 <= rel(e["t"]) <= hi * 1000):
            continue
        ev = e["ev"]
        if ev in ("propose", "block"):
            b = blk(e["hash"])
            b["slot"] = e["slot"]
            b["parent"] = e["parent"]
            b["parentSlot"] = e["parent_slot"]
            b["txs"] = e["txs"]
            if ev == "propose":
                b["proposer"] = n
                b["proposedAt"] = rel(e["t"])
            else:
                b.setdefault("proposer", by_short.get(e.get("proposer")))
                b["recv"][n] = rel(e["t"])
                if e.get("qc"):
                    b["parentQcSigners"] = sorted(by_short[s] for s in e["qc"] if s in by_short)
        elif ev == "vote":
            blk(e["hash"])["votes"][n] = rel(e["t"])
        elif ev == "qc":
            b = blk(e["hash"])
            b["qc"][n] = rel(e["t"])
            b["qcSigners"] = sorted(by_short[s] for s in e["signers"] if s in by_short)
            b.setdefault("slot", e["slot"])
        elif ev == "final":
            b = blk(e["hash"])
            b["final"][n] = rel(e["t"])
            b.setdefault("slot", e["slot"])
        elif ev == "prune":
            for h in e["blocks"]:
                blk(h).setdefault("prunedAt", rel(e["t"]))

    out_blocks = []
    for b in blocks.values():
        if "slot" not in b or "proposer" not in b or b.get("proposer") is None:
            continue
        times = [b.get("proposedAt")] + list(b["recv"].values())
        times = [t for t in times if t is not None]
        b["seen"] = min(times) if times else None
        b["finalAt"] = min(b["final"].values()) if b["final"] else None
        b["qcAt"] = min(b["qc"].values()) if b["qc"] else None
        out_blocks.append(b)
    out_blocks.sort(key=lambda b: (b["slot"], b["hash"]))

    finals = [b for b in out_blocks if b["finalAt"] is not None and b.get("proposedAt") is not None]
    lat = sorted(b["finalAt"] - b["proposedAt"] for b in finals)

    doc = {
        "source": "Aether 4-validator devnet, 4 processes on one Apple M4 (localhost)",
        "scenario": "100 tx/s of transfers for 90 s; validator 3 killed at t=25 s, restarted at t=55 s",
        "slotMs": genesis.get("chain", {}).get("slot_ms", 500),
        "startMs": t0,
        "window": [lo * 1000, hi * 1000],
        "validators": validators,
        "down": [{"validator": sched["killedValidator"], "from": rel(sched["killMs"]), "to": rel(sched["restartMs"])}],
        "blocks": out_blocks,
        "summary": {
            "blocks": len(out_blocks),
            "finalized": len(finals),
            "pruned": sum(1 for b in out_blocks if "prunedAt" in b),
            "proposeToFinalMsP50": lat[len(lat) // 2] if lat else None,
        },
    }
    with open(out, "w") as f:
        json.dump(doc, f, separators=(",", ":"))
    print(f"wrote {out}: {len(out_blocks)} blocks, {os.path.getsize(out)} bytes")


if __name__ == "__main__":
    main()
