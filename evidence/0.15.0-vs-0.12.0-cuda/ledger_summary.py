#!/usr/bin/env python3
"""Server-side aggregates from `lambo serve --ledger` files, for the 0.15.0 gate.

The harness files (`conc*.json`, `pair*.json`) time each call at the client.
This reads each serve's own ledger instead, where a tool line carries the
handler's `duration_us` and `ts` (the end of the call) and a `completion` line
marks a write applied. It emits numbers and timestamps only: no query text, no
concept text, no receipt ids and no paths.

Usage:
    python3 ledger_summary.py [--pair A,B]... LABEL=serve-ledger.jsonl... > ledger-summary.json

Definitions, used by the README:
- recall: `lambo_recall` calls that ended more than 5 s after the run's first
  call (the first recall of a fresh serve is cold).
- ack: `lambo_derive` and `lambo_record_action` calls (the handler returns once
  the write is validated and ordered).
- apply lag: a write's `completion` time minus the end of its ack.
- slow call: a recall or ack of 100 ms or more that ended more than 5 s after
  the first call. `lambo_stats` is left out because a receipt query may wait
  by design.
- percentiles: nearest rank, the same rule as the A/B harness.
"""
import argparse
import collections
import datetime as dt
import json
import sys

SLOW_MS = 100.0
COLD_S = 5.0
LOAD_TOOLS = ("lambo_recall", "lambo_derive", "lambo_record_action")
ACK_TOOLS = ("lambo_derive", "lambo_record_action")


def epoch(ts):
    return dt.datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()


def utc(t):
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def dist(xs):
    if not xs:
        return {"n": 0}
    s = sorted(xs)
    q = lambda p: s[min(len(s) - 1, int(round(p * (len(s) - 1))))]
    return {"n": len(s), "p50": round(q(.5), 2), "p90": round(q(.9), 2),
            "p99": round(q(.99), 2), "max": round(s[-1], 2)}


def read(path):
    rows = []
    with open(path) as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except ValueError:
                pass
    return rows


def summarize(rows):
    calls = [(epoch(o["ts"]), o["tool"], o["duration_us"] / 1000.0, o.get("receipt"))
             for o in rows if o.get("tool") and o.get("duration_us") is not None]
    t0 = min(c[0] for c in calls)
    load = [c for c in calls if c[1] in LOAD_TOOLS]
    recall = [c[2] for c in load if c[1] == "lambo_recall" and c[0] - t0 > COLD_S]
    ack = [c[2] for c in load if c[1] in ACK_TOOLS]
    acked_at = {c[3]: c[0] for c in load if c[1] in ACK_TOOLS and c[3]}
    lag = []
    lag_by_bucket = collections.defaultdict(list)
    for o in rows:
        if o.get("kind") == "completion" and o.get("receipt") in acked_at:
            t = epoch(o["ts"])
            ms = (t - acked_at[o["receipt"]]) * 1000.0
            lag.append(ms)
            lag_by_bucket[int((t - t0) // 10)].append(ms)
    slow = [c for c in load if c[2] >= SLOW_MS and c[0] - t0 > COLD_S]
    slow_seconds = sorted({int(c[0]) for c in slow})
    per_second_max = collections.defaultdict(float)
    for c in load:
        per_second_max[int(c[0])] = max(per_second_max[int(c[0])], c[2])
    buckets = collections.defaultdict(lambda: collections.defaultdict(list))
    for c in load:
        buckets[int((c[0] - t0) // 10)][c[1]].append(c[2])
    bucket_rows = []
    for k in sorted(buckets):
        b = buckets[k]
        acks = b["lambo_derive"] + b["lambo_record_action"]
        lags = lag_by_bucket.get(k, [])
        bucket_rows.append({
            "t_s": k * 10,
            "load_calls": sum(len(b[t]) for t in LOAD_TOOLS),
            "recall_p50_ms": dist(b["lambo_recall"]).get("p50"),
            "recall_max_ms": dist(b["lambo_recall"]).get("max"),
            "ack_max_ms": dist(acks).get("max"),
            "completions": len(lags),
            "apply_lag_p50_ms": dist(lags).get("p50"),
            "apply_lag_max_ms": dist(lags).get("max"),
        })
    return {
        "first_call_utc": utc(t0),
        "last_load_call_utc": utc(max(c[0] for c in load)),
        "calls_by_tool": dict(collections.Counter(c[1] for c in calls)),
        "completions": len(lag),
        "recall_ms": dist(recall),
        "ack_ms": dist(ack),
        "apply_lag_ms": dist(lag),
        "slow_calls": len(slow),
        "slow_call_max_ms": round(max((c[2] for c in slow), default=0.0), 2),
        "slow_seconds": len(slow_seconds),
        "slow_seconds_utc": [utc(s) for s in slow_seconds],
        "buckets_10s": bucket_rows,
    }, {"t0": t0, "slow_seconds": set(slow_seconds), "per_second_max": dict(per_second_max)}


def compare(a, b, ia, ib):
    """Second-by-second comparison of two runs made at the same time."""
    start = max(ia["t0"], ib["t0"]) + COLD_S
    seconds = sorted(s for s in set(ia["per_second_max"]) & set(ib["per_second_max"]) if s > start)
    ma, mb = ia["per_second_max"], ib["per_second_max"]
    at = {}
    for thr in (50, 100, 250, 500):
        at[str(thr)] = {
            a: sum(1 for s in seconds if ma[s] >= thr),
            b: sum(1 for s in seconds if mb[s] >= thr),
            "both": sum(1 for s in seconds if ma[s] >= thr and mb[s] >= thr),
        }
    diffs = sorted(mb[s] - ma[s] for s in seconds)
    near = lambda xs, ys: {x for x in xs if any(abs(x - y) <= 1 for y in ys)}
    sa, sb = ia["slow_seconds"], ib["slow_seconds"]
    return {
        "a": a, "b": b, "seconds_compared": len(seconds),
        "seconds_with_slowest_call_at_least_ms": at,
        "per_second_slowest_b_minus_a_median_ms": round(diffs[len(diffs) // 2], 2) if diffs else None,
        "seconds_b_slower_by_over_5ms": sum(1 for d in diffs if d > 5),
        "seconds_a_slower_by_over_5ms": sum(1 for d in diffs if d < -5),
        "slow_seconds": {a: len(sa), b: len(sb)},
        "slow_seconds_shared_within_1s": {a: len(near(sa, sb)), b: len(near(sb, sa))},
        "slow_seconds_only": {a: len(sa - near(sa, sb)), b: len(sb - near(sb, sa))},
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pair", action="append", default=[], help="A,B: two labels run at the same time")
    ap.add_argument("ledger", nargs="+", help="LABEL=path to a serve ledger")
    o = ap.parse_args()
    runs, internal = {}, {}
    for spec in o.ledger:
        label, path = spec.split("=", 1)
        runs[label], internal[label] = summarize(read(path))
    pairs = []
    for spec in o.pair:
        a, b = spec.split(",", 1)
        pairs.append(compare(a, b, internal[a], internal[b]))
    json.dump({"slow_ms": SLOW_MS, "cold_s": COLD_S, "runs": runs, "pairs": pairs},
              sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
