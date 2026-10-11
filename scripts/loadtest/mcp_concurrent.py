#!/usr/bin/env python3
"""Concurrent mixed-workload driver for one `lambo serve` (streamable HTTP).

    python3 scripts/loadtest/mcp_concurrent.py \
        --endpoint http://127.0.0.1:17701/mcp --out conc-w8.json \
        [--workers 8] [--secs 120] [--think 1.0] [--seed 1] \
        [--mix recall=40,derive=38,record_action=18,stats=4] \
        [--queries queries.txt] [--server-ledger ledger.jsonl]

N workers each hold their own MCP session and agent id. Between calls a worker
waits an exponentially distributed think time (mean --think seconds), then
picks a tool by the --mix weights. The default mix is the live dogfood
ledger's. After --secs the workers stop, the driver waits for the write queue
to drain (bounded by --drain-timeout), and then asks `lambo_stats(receipt)` for
the outcome of every acknowledged write it collected (or a seeded sample of at
most --receipt-max of them), using the session of the worker that owns the
receipt: receipts are scoped to the agent that created them.

What it reports (JSON of aggregates only, no memory content, safe to paste
into an issue):

* per-tool latency (p50/p90/p99/mean/max, via mcp_ab.summ) and call errors;
* `writes`: acked, refused at ack, receipts queried, receipt states, the
  failure rate among settled receipts, and failed writes grouped by reason.
  A receipt's reason is the model-safe class the server puts on it (N4, for
  example "conflict" or "internal error"); with --server-ledger the raw
  operator-facing `error` of each failed `completion` line for one of our
  receipts is grouped too (numbers and URLs normalized away). This is the
  nrynss/lambo#116 acceptance metric: failure rate and reason under N workers;
* `drain`: `drained` true only when the queue emptied before --drain-timeout.
  With `drained: false` the counters are a snapshot, not totals;
* `final_stats`: the server's write-queue counters after the drain.

Determinism: --seed drives every worker's RNG (`random.Random(seed * 1000 +
worker)`), so the call mix and every derive / record_action text are a pure
function of (seed, worker, call number). Timing, and so how many calls each
worker reaches, is not.

The driver writes synthetic derives, so point it at a scratch serve on a store
copy, never a live writer: port 7700 is refused unless --allow-production-port
is passed. Stdlib only; reuses mcp_ab.py's MCP client and summary.
"""
from __future__ import annotations

import argparse
import collections
import json
import os
import random
import re
import sys
import threading
import time
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_ab  # noqa: E402  (sibling module; path set above)

DEFAULT_ENDPOINT = "http://127.0.0.1:17700/mcp"
PRODUCTION_PORT = mcp_ab.PRODUCTION_PORT
DEFAULT_MIX = "recall=40,derive=38,record_action=18,stats=4"
TOOLS = {"recall": "lambo_recall", "derive": "lambo_derive",
         "record_action": "lambo_record_action", "stats": "lambo_stats"}
WRITE_TOOLS = ("lambo_derive", "lambo_record_action")
SETTLED_FAILURE_STATES = ("failed", "dropped")
FAILED_PREFIXES = ("FAILED, nothing was written — ", "DROPPED before it was attempted, "
                   "nothing was written — ")


# ---------------------------------------------------------------------------
# Pure helpers (unit-tested in test_loadtest_harnesses.py)
# ---------------------------------------------------------------------------

def parse_mix(spec: str) -> list[tuple[str, float]]:
    """`recall=40,derive=38` -> [("lambo_recall", 40.0), ("lambo_derive", 38.0)].

    Names are the short tool names in TOOLS (the `lambo_` prefix is accepted
    too). Unknown names, negative weights, duplicates and an all-zero mix are
    errors (ValueError).
    """
    out: dict[str, float] = {}
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        name, sep, weight = part.partition("=")
        name = name.strip().removeprefix("lambo_")
        if not sep or name not in TOOLS:
            raise ValueError(f"bad mix entry {part!r}: expected NAME=WEIGHT with NAME in "
                             f"{', '.join(TOOLS)}")
        try:
            w = float(weight)
        except ValueError:
            raise ValueError(f"bad weight in {part!r}") from None
        if w < 0:
            raise ValueError(f"negative weight in {part!r}")
        if TOOLS[name] in out:
            raise ValueError(f"duplicate mix entry for {name}")
        out[TOOLS[name]] = w
    if not out or sum(out.values()) <= 0:
        raise ValueError(f"mix {spec!r} has no positive weight")
    return list(out.items())


def derive_concepts(seed: int, worker: int, n: int, query: str) -> list[dict]:
    """The two concepts one synthetic derive writes. Pure in its arguments."""
    head = query.split()[0] if query.split() else "x"
    return [
        {"content": f"conc-s{seed}-w{worker}-{n}: note about {query}",
         "concept_type": "observation"},
        {"content": f"decision s{seed}.{worker}.{n} on {head} handling",
         "concept_type": "logic"},
    ]


def action_args(seed: int, worker: int, n: int, query: str, file_no: int) -> dict:
    """The arguments of one synthetic record_action. Pure in its arguments."""
    return {"action": f"conc-s{seed}-w{worker}-{n} touched {query[:40]}",
            "modifies": [f"file-{file_no}.rs"]}


_URL = re.compile(r"[a-z][a-z0-9+.-]*://\S+", re.I)
_PATH = re.compile(r"(?<![\w.])/(?:[\w.-]+/)+[\w.-]*")
_NUM = re.compile(r"\d+(?:\.\d+)?")
_RECEIPT = re.compile(r"\blwr\S+")


def normalize_reason(text: str | None, limit: int = 160) -> str:
    """A grouping key for a failure reason: first line, known prefix dropped,
    URLs, absolute paths, receipt ids and numbers replaced, length bounded.

    Numbers are folded so "after 8 concurrent graph changes" and a timeout
    duration group together across runs; the point is the class of failure.
    """
    if not text:
        return "(no reason given)"
    line = text.strip().splitlines()[0] if text.strip() else ""
    for p in FAILED_PREFIXES:
        if line.startswith(p):
            line = line[len(p):]
    line = _URL.sub("<url>", line)
    line = _PATH.sub("<path>", line)
    line = _RECEIPT.sub("<receipt>", line)
    line = _NUM.sub("N", line)
    line = re.sub(r"\s+", " ", line).strip()
    if len(line) > limit:
        line = line[:limit - 1] + "…"
    return line or "(no reason given)"


def pick_sample(items: list, k: int, seed: int) -> list:
    """All items when k <= 0 or k >= len(items); otherwise a seeded sample of
    k, returned in the original order."""
    if k <= 0 or k >= len(items):
        return list(items)
    idx = sorted(random.Random(seed).sample(range(len(items)), k))
    return [items[i] for i in idx]


def receipt_outcome(stats_body: dict | None, is_error: bool, text: str = "") -> dict:
    """Reduce one lambo_stats(receipt) reply to {"state", "reason"}."""
    if is_error or not isinstance(stats_body, dict):
        return {"state": "query_error", "reason": normalize_reason(text)}
    r = stats_body.get("receipt")
    if not isinstance(r, dict):
        return {"state": "unknown", "reason": None}
    state = r.get("state") or "unknown"
    reason = normalize_reason(r.get("detail")) if state in SETTLED_FAILURE_STATES else None
    return {"state": state, "reason": reason}


def summarize_writes(acked: int, refused: list[str], outcomes: list[dict],
                     ledger_errors: list[str] | None = None) -> dict:
    """Aggregate write outcomes.

    acked: writes the server acknowledged with a receipt.
    refused: normalized reasons of writes refused at ack (isError).
    outcomes: receipt_outcome() dicts for the queried receipts.
    failure_rate is failed+dropped over settled queried receipts (pending,
    expired and query errors are not settled outcomes and are excluded); None
    when nothing settled.
    """
    states = collections.Counter(o["state"] for o in outcomes)
    failed = [o for o in outcomes if o["state"] in SETTLED_FAILURE_STATES]
    unsettled = {"pending", "pending_replay", "expired", "query_error", "unknown",
                 "restart_lost", "never_issued", "forbidden", "intent_durable"}
    settled = sum(n for s, n in states.items() if s not in unsettled)
    by_reason = collections.Counter(f"{o['state']}: {o['reason']}" for o in failed)
    out = {
        "acked": acked,
        "refused_at_ack": len(refused),
        "refused_at_ack_reasons": dict(collections.Counter(refused).most_common()),
        "receipts_queried": len(outcomes),
        "receipt_states": dict(states.most_common()),
        "settled": settled,
        "failed": len(failed),
        "failure_rate": round(len(failed) / settled, 4) if settled else None,
        "failed_reasons": dict(by_reason.most_common()),
    }
    if ledger_errors is not None:
        out["failed_reasons_server_ledger"] = dict(
            collections.Counter(ledger_errors).most_common())
    return out


def ledger_failure_reasons(lines, receipts: set[str]) -> list[str]:
    """Normalized raw `error` of each failed `completion` line whose receipt is
    one of ours. `lines` is an iterable of JSONL strings; bad lines are skipped."""
    out = []
    for line in lines:
        if '"completion"' not in line:
            continue
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if (isinstance(d, dict) and d.get("kind") == "completion"
                and d.get("state") == "failed" and d.get("receipt") in receipts):
            out.append(normalize_reason(str(d.get("error") or "")))
    return out


def drain_done(stats: dict) -> bool:
    return (stats.get("write_queue_outstanding", 0) == 0 and stats.get("log_depth", 0) == 0
            and stats.get("flush_depth", 0) == 0)


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

class Client(mcp_ab.Mcp):
    """mcp_ab.Mcp with a per-worker agent id and a non-raising call."""

    def __init__(self, url, agent_id, token):
        self.agent_id = agent_id
        super().__init__(url, token)

    def call(self, name, **args):
        """(ms, is_error, structured body or None, text)."""
        args["agent_id"] = self.agent_id
        t = time.perf_counter()
        try:
            res = self._rpc("tools/call", {"name": name, "arguments": args})
        except Exception as e:  # transport or JSON-RPC error
            return (time.perf_counter() - t) * 1000, True, None, f"transport: {type(e).__name__}"
        ms = (time.perf_counter() - t) * 1000
        text = "\n".join(c.get("text", "") for c in res.get("content", []) or []
                         if isinstance(c, dict))
        return ms, bool(res.get("isError")), res.get("structuredContent"), text


def run(o, queries, mix, token):
    lat = collections.defaultdict(list)
    errs = collections.Counter()
    refused: list[str] = []
    receipts: list[tuple[int, str]] = []  # (worker, receipt id)
    lock = threading.Lock()
    stop = time.time() + o.secs
    clients = [Client(o.endpoint, f"conc-{i}", token) for i in range(o.workers)]
    tools, weights = [m[0] for m in mix], [m[1] for m in mix]

    def worker(i):
        rng = random.Random(o.seed * 1000 + i)
        c = clients[i]
        n = 0
        while time.time() < stop:
            time.sleep(rng.expovariate(1 / o.think) if o.think > 0 else 0)
            if time.time() >= stop:
                break
            tool = rng.choices(tools, weights)[0]
            n += 1
            q = rng.choice(queries)
            if tool == "lambo_recall":
                r = c.call(tool, query=q)
            elif tool == "lambo_derive":
                r = c.call(tool, concepts=derive_concepts(o.seed, i, n, q))
            elif tool == "lambo_record_action":
                r = c.call(tool, **action_args(o.seed, i, n, q, rng.randrange(40)))
            else:
                r = c.call(tool)
            ms, is_err, body, text = r
            with lock:
                lat[tool].append(ms)
                errs[tool] += is_err
                if tool in WRITE_TOOLS:
                    rid = (body or {}).get("receipt") if not is_err else None
                    if isinstance(rid, str):
                        receipts.append((i, rid))
                    else:
                        refused.append(normalize_reason(text))

    warm = Client(o.endpoint, "conc-warm", token)
    for q in queries[:3]:
        warm.call("lambo_recall", query=q)
    ts = [threading.Thread(target=worker, args=(i,)) for i in range(o.workers)]
    t0 = time.time()
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    wall = time.time() - t0

    drain_t = time.time()
    st, drained = {}, False
    while True:
        _, is_err, body, _ = warm.call("lambo_stats")
        st = body if isinstance(body, dict) and not is_err else st
        if st and drain_done(st):
            drained = True
            break
        if time.time() - drain_t > o.drain_timeout:
            break
        time.sleep(0.5)
    drain_s = time.time() - drain_t

    sample = pick_sample(receipts, o.receipt_max, o.seed)
    outcomes = []
    for wi, rid in sample:
        _, is_err, body, text = clients[wi].call("lambo_stats", receipt=rid, wait_ms=0)
        outcomes.append(receipt_outcome(body, is_err, text))

    ledger_errors = None
    if o.server_ledger:
        mine = {rid for _, rid in receipts}
        with open(o.server_ledger, errors="replace") as f:
            ledger_errors = ledger_failure_reasons(f, mine)

    keep = {k: st.get(k) for k in st
            if k.startswith("write_queue_") or k in (
                "dead_lettered", "degraded", "concept_count", "embedded_concepts")}
    writes = summarize_writes(len(receipts), refused, outcomes, ledger_errors)
    writes["sampled"] = len(sample) < len(receipts)
    return {
        "workers": o.workers, "secs": o.secs, "think": o.think, "seed": o.seed,
        "mix": {t: w for t, w in mix},
        "wall_s": round(wall, 1),
        "drain": {"drained": drained, "drain_s": round(drain_s, 1),
                  "drain_timeout_s": o.drain_timeout},
        "calls": sum(len(v) for v in lat.values()),
        "latency_ms": {k: mcp_ab.summ(v) for k, v in sorted(lat.items())},
        "errors": dict(errs),
        "writes": writes,
        "final_stats": keep,
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT,
                    help=f"streamable-HTTP MCP endpoint (default {DEFAULT_ENDPOINT})")
    ap.add_argument("--out", required=True, help="JSON output path (aggregates only)")
    ap.add_argument("--queries", help="recall queries, one per line "
                    "(default: mcp_ab.py's built-in list)")
    ap.add_argument("--workers", type=int, default=8)
    ap.add_argument("--secs", type=float, default=120.0)
    ap.add_argument("--think", type=float, default=1.0,
                    help="mean think time between a worker's calls, seconds")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--mix", default=DEFAULT_MIX,
                    help=f"tool weights as NAME=WEIGHT,... with NAME in "
                         f"{', '.join(TOOLS)} (default {DEFAULT_MIX})")
    ap.add_argument("--drain-timeout", type=float, default=300.0,
                    help="seconds to wait for the write queue to empty after the run")
    ap.add_argument("--receipt-max", type=int, default=2000,
                    help="query at most this many receipts after the drain (seeded "
                         "sample when more were acked; 0 = all)")
    ap.add_argument("--server-ledger",
                    help="the serve's --ledger file, read only, to group the raw "
                         "operator-facing error of each failed write of this run")
    ap.add_argument("--token", help="bearer token; defaults to $LAMBO_AUTH_TOKEN")
    ap.add_argument("--allow-production-port", action="store_true",
                    help=f"permit an endpoint on port {PRODUCTION_PORT}, where a live writer listens")
    o = ap.parse_args(argv)

    try:
        port = urllib.parse.urlsplit(o.endpoint).port
    except ValueError as e:
        sys.exit(f"error: --endpoint {o.endpoint!r} has an invalid port: {e}")
    if port == PRODUCTION_PORT and not o.allow_production_port:
        sys.exit(f"refusing {o.endpoint}: port {PRODUCTION_PORT} is where a live lambo serve "
                 "writer listens, and this driver writes synthetic derives. Point it at a "
                 "scratch serve, or pass --allow-production-port.")
    if o.workers < 1 or o.secs <= 0 or o.think < 0:
        sys.exit("error: --workers must be >= 1, --secs > 0 and --think >= 0")
    try:
        mix = parse_mix(o.mix)
    except ValueError as e:
        sys.exit(f"error: --mix: {e}")
    if o.queries:
        queries = [ln.strip() for ln in open(o.queries) if ln.strip()]
        if not queries:
            sys.exit(f"error: {o.queries} has no queries")
    else:
        queries = list(mcp_ab.QUERIES)

    res = run(o, queries, mix, o.token or os.environ.get("LAMBO_AUTH_TOKEN"))
    with open(o.out, "w") as f:
        json.dump(res, f, indent=2)
    print(json.dumps(res, indent=2))
    if not res["drain"]["drained"]:
        print(f"warning: the write queue did not drain within {o.drain_timeout:g}s; "
              "counts are a snapshot, not totals", file=sys.stderr)


if __name__ == "__main__":
    main()
