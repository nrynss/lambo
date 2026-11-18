#!/usr/bin/env python3
"""Probe the post-settle window: how long after a write's receipt reads `applied`
does the NEXT derive's ack stay slow? One serve, sequential, no interleave.

Usage: ack_window.py http://127.0.0.1:PORT/mcp OUT.json PATH/TO/scripts/loadtest
Run it against a scratch serve on a store copy: it writes synthetic derives.
"""
import sys, time, json, statistics
sys.path.insert(0, sys.argv[3])  # path to scripts/loadtest
import mcp_ab
url, out = sys.argv[1], sys.argv[2]
c = mcp_ab.Mcp(url)
for q in mcp_ab.QUERIES[:3]:
    c.tool("lambo_recall", query=q)
def q(xs, p):
    s = sorted(xs); return round(s[min(len(s)-1, int(round(p*(len(s)-1))))], 2)
res = {}
for gap in (0, 5, 10, 20, 40, 80, 160):
    acks, waits = [], []
    for i in range(25):
        content = f"ack-window probe gap{gap} #{i}: timing observation {time.time_ns()}"
        ms, body, err = c.tool("lambo_derive", concepts=[{"content": content, "concept_type": "observation"}])
        t = time.perf_counter()
        _, st, _ = c.tool("lambo_stats", receipt=body["receipt"], wait_ms=4000)
        waits.append((time.perf_counter() - t) * 1000)
        assert (st.get("receipt") or {}).get("state") == "applied", st.get("receipt")
        if i:  # first ack of a gap follows the previous gap's sleep
            acks.append(ms)
        time.sleep(gap / 1000)
    res[gap] = {"ack_p50": q(acks, .5), "ack_p90": q(acks, .9), "ack_max": round(max(acks), 2),
                "wait_applied_p50": q(waits, .5)}
    print(f"gap {gap:>3} ms  ack p50 {res[gap]['ack_p50']:>7}  p90 {res[gap]['ack_p90']:>7}  max {res[gap]['ack_max']:>7}   receipt wait p50 {res[gap]['wait_applied_p50']}", flush=True)
json.dump(res, open(out, "w"), indent=1)
