#!/usr/bin/env python3
"""Summarize scripts/bench/run-benchmarks.sh output into summary.json and a
markdown table. usage: summarize.py <results-dir>"""
import glob
import json
import os
import statistics
import sys

res = sys.argv[1]
out = {"transfer": [], "contract": []}
for mode in ("transfer", "contract"):
    for f in sorted(glob.glob(os.path.join(res, f"{mode}-*.json")), key=lambda p: int(p.rsplit("-", 1)[1][:-5])):
        d = json.load(open(f))
        out[mode].append({
            "offered": d["offeredRateTps"],
            "submitted": d["accepted"],
            "finalized": d["finalized"],
            "lost": d["notFinalizedAfterDrain"],
            "finalizedTpsSteadyWindow": round(d["finalizedTpsSteadyWindow"], 1),
            "p50": d["finalityMs"]["p50"], "p95": d["finalityMs"]["p95"],
            "p99": d["finalityMs"]["p99"], "max": d["finalityMs"]["max"],
            "meanTxsPerBlock": round(d["blocks"]["meanTxsPerBlock"], 1),
        })

# idle network: block interval and propose->final latency from validator 1's event log
ev = [json.loads(l) for l in open(os.path.join(res, "idle-node1.events.jsonl"))]
blocks = {}
for e in ev:
    if e["ev"] in ("propose", "block"):
        blocks.setdefault(e["hash"], {}).update(slot=e["slot"], seen=min(e["t"], blocks.get(e["hash"], {}).get("seen", e["t"])))
    if e["ev"] == "final":
        blocks.setdefault(e["hash"], {})["final"] = e["t"]
fin = sorted((b for b in blocks.values() if "final" in b and "seen" in b), key=lambda b: b["slot"])
slots = [b["slot"] for b in fin]
lat = sorted(b["final"] - b["seen"] for b in fin)
span_slots = max(slots) - min(slots) if slots else 0
intervals = [ (b - a) * 500 for a, b in zip(slots, slots[1:]) ]
q = lambda v, p: v[min(len(v) - 1, int(round(p / 100 * (len(v) - 1))))] if v else None
out["idle"] = {
    "slotMs": 500,
    "slotsObserved": span_slots,
    "finalizedBlocks": len(fin),
    "blocksPerSlot": round(len(fin) / span_slots, 3) if span_slots else None,
    "meanFinalizedBlockIntervalMs": round(statistics.mean(intervals)) if intervals else None,
    "seenToFinalMs": {"p50": q(lat, 50), "p95": q(lat, 95), "p99": q(lat, 99)},
}

# liveness: finality progress while validator 3 was down
sched = json.load(open(os.path.join(res, "liveness-schedule.json")))
lev = []
for f in glob.glob(os.path.join(res, "liveness-events", "node*.events.jsonl")):
    lev += [json.loads(l) for l in open(f)]
finals = {}
for e in lev:
    if e["ev"] == "final":
        finals.setdefault(e["hash"], {})[e["node"]] = e["t"]
first_final = sorted(min(v.values()) for v in finals.values())
def count(a, b):
    return sum(1 for t in first_final if a <= t < b)
k, r = sched["killMs"], sched["restartMs"]
gaps_down = [b - a for a, b in zip([k] + [t for t in first_final if k <= t < r], [t for t in first_final if k <= t < r] + [r])]
# when validator 3 (the killed one) finalized again after restart, and caught up
genesis = json.load(open(os.path.join(res, "liveness-events", "genesis.json")))
import hashlib
short3 = hashlib.sha256(bytes(genesis["validators"][2]["pubkey"])).digest()[:4].hex()
v3_after = sorted(t for v in finals.values() for n, t in v.items() if n == short3 and t >= r)
net_final_before_restart = sorted(t for t in first_final if t < r)
caught = None
if v3_after:
    # first time v3 finalizes a block the network finalized after the restart (i.e. it is live again)
    for v in finals.values():
        pass
out["liveness"] = {
    "schedule": {"killAfterS": round((k - sched["startMs"]) / 1000, 1), "restartAfterS": round((r - sched["startMs"]) / 1000, 1)},
    "finalizedBlocks": {"before": count(sched["startMs"], k), "whileDown": count(k, r), "afterRestart": count(r, r + 30000)},
    "durationsS": {"before": round((k - sched["startMs"]) / 1000, 1), "whileDown": round((r - k) / 1000, 1), "afterRestart": 30},
    "maxGapBetweenFinalizedBlocksWhileDownMs": max(gaps_down) if gaps_down else None,
    "restartedValidatorFirstFinalAfterRestartMs": (v3_after[0] - r) if v3_after else None,
    "load": json.load(open(os.path.join(res, "liveness-load.json"))),
}
for key in ("rejectSamples",):
    out["liveness"]["load"].pop(key, None)
bls = json.load(open(os.path.join(res, "bls.json")))["runs"]
med = lambda xs: statistics.median(xs)
out["bls"] = {
    "signatures": bls[0]["signatures"],
    "runs": len(bls),
    "singleVerifyPerSec": round(med([r["singleVerify"]["sigsPerSec"] for r in bls])),
    "batchVerifyPerSec": round(med([r["batchVerifyDistinctMessages"]["sigsPerSec"] for r in bls])),
    "aggregateSameMessagePerSec": round(med([r["aggregateSameMessage"]["sigsPerSec"] for r in bls])),
    "batchSpeedup": round(med([r["batchSpeedup"] for r in bls]), 2),
}
json.dump(out, open(os.path.join(res, "summary.json"), "w"), indent=2)
print(json.dumps(out, indent=2))
