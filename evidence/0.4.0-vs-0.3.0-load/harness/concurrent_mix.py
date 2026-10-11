"""Concurrent mixed-workload latency driver for one lambo serve (HTTP).
python3 conc.py URL QUERIES_FILE OUT.json [--workers 8] [--secs 120] [--think 1.0] [--seed 1]
"""
import sys, json, random, threading, time, argparse
import pathlib; sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[3] / "scripts" / "loadtest"))
import mcp_ab
ap = argparse.ArgumentParser()
ap.add_argument("url"); ap.add_argument("queries"); ap.add_argument("out")
ap.add_argument("--workers", type=int, default=8); ap.add_argument("--secs", type=float, default=120)
ap.add_argument("--think", type=float, default=1.0); ap.add_argument("--seed", type=int, default=1)
o = ap.parse_args()
qs = [l.strip() for l in open(o.queries) if l.strip()]
MIX = [("lambo_recall", 40), ("lambo_derive", 38), ("lambo_record_action", 18), ("lambo_stats", 4)]
lat, errs, lock = {}, {}, threading.Lock()
class W(mcp_ab.Mcp):
    def __init__(self, url, aid):
        self.aid = aid; super().__init__(url)
    def call(self, name, **args):
        args["agent_id"] = self.aid
        t = time.perf_counter()
        try:
            res = self._rpc("tools/call", {"name": name, "arguments": args}); err = bool(res.get("isError"))
        except Exception as e:
            err = True
        return (time.perf_counter() - t) * 1000, err
def worker(i, stop):
    rng = random.Random(o.seed * 1000 + i)
    c = W(o.url, f"conc-{i}")
    n = 0
    while time.time() < stop:
        time.sleep(rng.expovariate(1 / o.think))
        tool = rng.choices([m[0] for m in MIX], [m[1] for m in MIX])[0]
        n += 1
        if tool == "lambo_recall":
            ms, e = c.call(tool, query=rng.choice(qs))
        elif tool == "lambo_derive":
            q = rng.choice(qs)
            ms, e = c.call(tool, concepts=[{"content": f"conc{i}-{n}: note about {q}", "concept_type": "observation"},
                                           {"content": f"decision {i}.{n} on {q.split()[0] if q.split() else 'x'} handling", "concept_type": "logic"}])
        elif tool == "lambo_record_action":
            ms, e = c.call(tool, action=f"conc{i}-{n} touched {rng.choice(qs)[:40]}", modifies=[f"file-{rng.randrange(40)}.rs"])
        else:
            ms, e = c.call(tool)
        with lock:
            lat.setdefault(tool, []).append(ms); errs[tool] = errs.get(tool, 0) + e
# warm-up
w0 = W(o.url, "conc-warm")
for q in qs[:3]: w0.call("lambo_recall", query=q)
stop = time.time() + o.secs
ts = [threading.Thread(target=worker, args=(i, stop)) for i in range(o.workers)]
t0 = time.time(); [t.start() for t in ts]; [t.join() for t in ts]; wall = time.time() - t0
# drain: wait for write queue to empty
_, _ = w0.call("lambo_stats")
drain_t = time.time()
while True:
    r = w0._rpc("tools/call", {"name": "lambo_stats", "arguments": {"agent_id": "conc-warm"}})
    st = r.get("structuredContent") or json.loads(r["content"][0]["text"])
    if st.get("write_queue_outstanding", 0) == 0 and st.get("log_depth", 0) == 0 and st.get("flush_depth", 0) == 0: break
    if time.time() - drain_t > 300: break
    time.sleep(0.5)
drain = time.time() - drain_t
keep = {k: st.get(k) for k in st if k.startswith("write_queue_apply") or k in ("write_queue_applied", "write_queue_failed", "write_queue_dropped", "dead_lettered", "degraded", "concept_count", "embedded_concepts")}
res = {"workers": o.workers, "secs": o.secs, "think": o.think, "wall_s": round(wall, 1), "drain_s": round(drain, 1),
       "calls": sum(len(v) for v in lat.values()), "latency_ms": {k: mcp_ab.summ(v) for k, v in lat.items()},
       "errors": errs, "final_stats": keep}
json.dump(res, open(o.out, "w"), indent=2); print(json.dumps(res))
