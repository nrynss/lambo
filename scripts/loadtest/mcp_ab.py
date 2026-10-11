#!/usr/bin/env python3
"""A/B two `lambo serve` builds for latency, plus a before/after split of a call ledger.

    python3 scripts/loadtest/mcp_ab.py ab --a old=http://127.0.0.1:7701/mcp \
        --b new=http://127.0.0.1:7702/mcp [--rounds 10] [--derives 30] --out ab.json
    python3 scripts/loadtest/mcp_ab.py ledger --ledger calls.jsonl \
        --switch 2026-10-11T04:12:33Z [--days 14] --out ledger.json

`ab` drives two HTTP serves, each on its own copy of the same store. Recalls are
interleaved A/B per query so drift (GPU clocks, page cache) hits both sides
equally; calls are sequential, so this measures latency, not throughput. It also
writes synthetic `lambo_derive` concepts, so point it at scratch copies, never a
live writer: port 7700 is refused unless --allow-production-port is passed.
Top-k overlap between the sides is reported as a recall-quality check.

`ledger` splits a `serve --ledger` file at the re-pin time and summarizes
per-tool server-side latency on each side. It only reads the file.

Stdlib only. Output is JSON of aggregates, no memory content, so it is safe to
paste into a public issue. First used for nrynss/lambo#108 (0.4.0 vs bef53e6).
"""
import argparse, json, os, statistics, sys, time, urllib.parse, urllib.request
from collections import defaultdict
from datetime import datetime, timedelta

AGENT = "perf-ab"
PRODUCTION_PORT = 7700  # a live writer's port; see mcp_load.py
QUERIES = [
    "dogfood rig systemd writer restart", "recall latency profiling sweeps",
    "embedding contract BGE-M3 candle", "single writer lease TTL",
    "level B pluggability backend registry", "canonization swarm promotion policy",
    "sqlite schema provision migration", "http transport stdio blackout",
    "vector dimension store authoritative", "ledger heartbeat stats",
    "cockroach DSN verify-full", "release 0.4.0 notes",
    "garbage collection sweep interval", "write queue receipt applied",
    "elastic recall tier mirror", "agent doctrine session start hook",
    "blast radius warning dependents", "reserve soft lock concept",
    "fixture embedder tests", "web portal credential sessions",
]


class Mcp:
    def __init__(self, url, token=None):
        self.url, self.token, self.sid, self.n = url, token, None, 0
        self._rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": AGENT, "version": "1"}})
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _post(self, body):
        h = {"content-type": "application/json", "accept": "application/json, text/event-stream"}
        if self.sid:
            h["mcp-session-id"] = self.sid
        if self.token:
            h["authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(self.url, json.dumps(body).encode(), h)
        with urllib.request.urlopen(req, timeout=120) as r:
            self.sid = r.headers.get("mcp-session-id") or self.sid
            return r.read().decode()

    def _rpc(self, method, params):
        self.n += 1
        raw = self._post({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        for line in raw.splitlines():
            if line.startswith("data:") and line[5:].strip():
                msg = json.loads(line[5:])
                if msg.get("id") == self.n:
                    if "error" in msg:
                        raise RuntimeError(msg["error"])
                    return msg["result"]
        return json.loads(raw)["result"]

    def tool(self, name, **args):
        args["agent_id"] = AGENT
        t = time.perf_counter()
        res = self._rpc("tools/call", {"name": name, "arguments": args})
        ms = (time.perf_counter() - t) * 1000
        body = res.get("structuredContent")
        if body is None:
            body = json.loads(res["content"][0]["text"])
        return ms, body, bool(res.get("isError"))


def summ(xs):
    if not xs:
        return {"n": 0}
    s = sorted(xs)
    q = lambda p: s[min(len(s) - 1, int(round(p * (len(s) - 1))))]
    return {"n": len(s), "p50": round(q(.5), 2), "p90": round(q(.9), 2),
            "p99": round(q(.99), 2), "mean": round(statistics.mean(s), 2),
            "max": round(s[-1], 2)}


def run_ab(a, b, rounds, derives, token):
    sides = {name: Mcp(url, token) for name, url in (a, b)}
    lat = {k: defaultdict(list) for k in sides}
    errors = {k: 0 for k in sides}
    hits = {k: {} for k in sides}
    for name, c in sides.items():  # warm-up: load model pages, caches
        for q in QUERIES[:3]:
            c.tool("lambo_recall", query=q)
    for _ in range(rounds):
        for q in QUERIES:
            for name, c in sides.items():
                ms, body, err = c.tool("lambo_recall", query=q, top_k=8)
                lat[name]["recall"].append(ms)
                errors[name] += err
                hits[name].setdefault(q, [h["node_id"] for h in body.get("hits", [])])
        for name, c in sides.items():
            ms, _, err = c.tool("lambo_stats")
            lat[name]["stats"].append(ms)
            errors[name] += err
    for i in range(derives):
        for name, c in sides.items():
            content = f"perf-ab synthetic observation {i}: write path timing probe {time.time_ns()}"
            ms, body, err = c.tool("lambo_derive",
                                   concepts=[{"content": content, "concept_type": "observation"}])
            lat[name]["derive_ack"].append(ms)
            errors[name] += err
            t = time.perf_counter()
            _, st, _ = c.tool("lambo_stats", receipt=body["receipt"], wait_ms=4000)
            lat[name]["derive_applied"].append(ms + (time.perf_counter() - t) * 1000)
            if st.get("receipt_state", st.get("receipt", {}).get("state")) not in (None, "applied"):
                errors[name] += 1
    names = list(sides)
    overlap = []
    for q in QUERIES:
        x, y = set(hits[names[0]].get(q, [])), set(hits[names[1]].get(q, []))
        if x or y:
            overlap.append(len(x & y) / len(x | y))
    return {"sides": names, "rounds": rounds, "derives": derives,
            "latency_ms": {n: {k: summ(v) for k, v in lat[n].items()} for n in names},
            "errors": errors,
            "recall_topk_jaccard": summ(overlap)}


def run_ledger(path, switch, days):
    sw = datetime.fromisoformat(switch.replace("Z", "+00:00"))
    lo = sw - timedelta(days=days)
    lat = {"old": defaultdict(list), "new": defaultdict(list)}
    err = {"old": defaultdict(int), "new": defaultdict(int)}
    flush = {"old": [], "new": []}
    with open(path) as f:
        for line in f:
            try:
                d = json.loads(line)
                ts = datetime.fromisoformat(d["ts"][:26] + "+00:00")
            except Exception:
                continue
            if ts < lo:
                continue
            side = "new" if ts >= sw else "old"
            if d.get("kind") == "call" or "duration_us" in d:
                if d.get("agent_id") == AGENT:
                    continue
                lat[side][d["tool"]].append(d["duration_us"] / 1000)
                err[side][d["tool"]] += d.get("outcome") != "ok"
            elif d.get("kind") == "stats":
                fl = d["stats"].get("flush_lag_ms")
                if fl is not None:
                    flush[side].append(fl)
    return {"window_days_before_switch": days, "switch": switch,
            "latency_ms": {s: {t: summ(v) for t, v in lat[s].items()} for s in lat},
            "error_count": {s: dict(err[s]) for s in err},
            "flush_lag_ms": {s: summ(flush[s]) for s in flush}}


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    ab = sub.add_parser("ab")
    ab.add_argument("--a", required=True)
    ab.add_argument("--b", required=True)
    ab.add_argument("--rounds", type=int, default=10)
    ab.add_argument("--derives", type=int, default=30)
    ab.add_argument("--token", help="bearer token; defaults to $LAMBO_AUTH_TOKEN")
    ab.add_argument("--allow-production-port", action="store_true",
                    help=f"permit an endpoint on port {PRODUCTION_PORT}, where a live writer listens")
    ab.add_argument("--out", required=True)
    lg = sub.add_parser("ledger")
    lg.add_argument("--ledger", required=True)
    lg.add_argument("--switch", required=True)
    lg.add_argument("--days", type=int, default=14)
    lg.add_argument("--out", required=True)
    o = p.parse_args()
    if o.cmd == "ab":
        a, b = (tuple(x.split("=", 1)) for x in (o.a, o.b))
        for _, url in (a, b):
            try:
                port = urllib.parse.urlsplit(url).port
            except ValueError as e:
                sys.exit(f"error: {url!r} has an invalid port: {e}")
            if port == PRODUCTION_PORT and not o.allow_production_port:
                sys.exit(f"refusing {url}: port {PRODUCTION_PORT} is where a live lambo serve "
                         "writer listens, and ab writes synthetic derives. Point it at a scratch "
                         "serve, or pass --allow-production-port.")
        r = run_ab(a, b, o.rounds, o.derives, o.token or os.environ.get("LAMBO_AUTH_TOKEN"))
    else:
        r = run_ledger(o.ledger, o.switch, o.days)
    json.dump(r, open(o.out, "w"), indent=2)
    print(json.dumps(r, indent=2))


if __name__ == "__main__":
    main()
