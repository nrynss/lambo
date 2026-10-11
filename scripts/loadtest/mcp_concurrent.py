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

* `latency_ms`: per-tool latency of successful calls (p50/p90/p99/mean/max,
  via mcp_ab.summ); `errors`: per tool, the count and latency of calls that
  came back as a tool error or a transport failure;
* `writes`: acked, refused at ack, transport-failed, receipts queried, receipt
  states, and failed writes grouped by reason. Two rates:
  - `failure_rate`: failed + dropped + close_unflushed over settled queried
    receipts (close_unflushed: the close's final flush did not complete, so
    the write may not have been saved; it counts as not applied);
  - `write_failure_rate_all`: (failed + dropped + close_unflushed + refused_at_ack +
    transport_failed) over (settled + refused_at_ack + transport_failed).
    This is the #116 acceptance rate. A transport-failed write may still have
    been admitted by the server (the reply was lost, not necessarily the
    write), so this rate is an upper bound on what the server failed;
* reasons are reduced to known-message ids (`hybrid_replan_exhausted`, ...)
  or the server's model-safe error class (`conflict`, `internal error`, ...);
  `--include-raw-reasons` keeps the normalized text instead. With
  --server-ledger, the raw operator-facing `error` of each failed
  `completion` line for one of our receipts is grouped the same way;
* `drain`: `drained` true only when the queue emptied before --drain-timeout.
  With `drained: false` the counters are a snapshot, not totals;
* `flags`: `expired_receipts` (receipts are kept RECEIPT_RETENTION_S = 300 s
  after they settle, so a long run + drain loses early outcomes) and
  `retention_risk` (run + drain reached RETENTION_WARN_FRACTION of that);
* `worker_errors`: workers that died on an unexpected exception;
* `final_stats`: the server's write-queue counters after the drain.

Exit status: 0 ok; 2 when the drain did not complete, no call succeeded, or a
worker died (the JSON is still written); other non-zero for usage errors.

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
SETTLED_FAILURE_STATES = ("failed", "dropped", "close_unflushed")
UNSETTLED_STATES = {"pending", "pending_replay", "expired", "query_error", "unknown",
                    "restart_lost", "never_issued", "forbidden", "intent_durable"}
FAILED_PREFIXES = ("FAILED, nothing was written — ", "DROPPED before it was attempted, "
                   "nothing was written — ")
# src/writeq/receipts.rs RECEIPT_RETENTION: a settled receipt is kept this long.
RECEIPT_RETENTION_S = 300.0
RETENTION_WARN_FRACTION = 0.8
EXIT_INCOMPLETE = 2

# Known server messages -> stable ids. Each pattern is anchored on the exact
# server text (cited), never a loose keyword, because a reason string can
# echo caller content.
KNOWN_MESSAGES = (
    # src/graph/hybrid.rs MAX_HYBRID_REPLANS failure
    ("hybrid_replan_exhausted",
     r"hybrid derive could not commit after \d+ concurrent graph changes"),
    # src/graph/hybrid.rs HYBRID_IO_TIMEOUT on the candidate lookup
    ("hybrid_candidate_timeout", r"hybrid vector candidate lookup timed out after "),
    # src/graph/hybrid.rs check_embed_budget
    ("hybrid_embed_budget",
     r"hybrid derive would embed \d+ new concepts and parent_of ends in one call"),
    # src/graph/hybrid.rs check_embed_context (#74), both messages
    ("hybrid_embed_context_cap",
     r"a hybrid derive embeds every new concept with it, so it may be at most \d+ bytes"
     r"|framed with the call's text for embedding \(the text, \" — \", then the call's "
     r"concepts joined by \"; \"\) is \d+ bytes; at most \d+ bytes fit"),
    # src/surface/validate.rs check_no_image_suffix (#22 PR 4)
    ("image_suffix_refused",
     r"may not contain an image suffix \(\[image:<id>\], in any case or spelling\)"),
    # src/surface/error.rs model_safe_message, embedding-contract refusal
    ("embedding_contract_mismatch",
     r"the session's embedding contract no longer matches this query"),
    # src/writeq/admission.rs DropReason::describe
    ("drop_lane_full", r"this agent's background write lane is full \(\d+ outstanding"),
    ("drop_queue_full", r"the background write queue is full \(\d+ outstanding"),
    ("drop_queue_bytes", r"the background write queue is at its \d+ MiB payload cap"),
    ("drop_session_closing", r"^the session is closing$"),
    # src/writeq/receipts.rs ReceiptAnswer::CloseUnflushed describe (#117)
    ("close_unflushed", r"^close did not finish its final flush"),
)
_KNOWN = tuple((rid, re.compile(p)) for rid, p in KNOWN_MESSAGES)
# src/surface/error.rs err_class: every model-safe failure starts with one.
ERROR_CLASSES = ("store error", "embedding error", "configuration error", "conflict",
                 "image id taken", "internal error")


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


def _strip_prefix(text: str) -> str:
    line = text.strip().splitlines()[0] if text and text.strip() else ""
    for p in FAILED_PREFIXES:
        if line.startswith(p):
            line = line[len(p):]
    return line


def normalize_reason(text: str | None, limit: int = 160) -> str:
    """Raw-mode grouping key: first line, known prefix dropped, URLs, absolute
    paths, receipt ids and numbers replaced, length bounded. Can still hold
    server or caller text; used only behind --include-raw-reasons."""
    line = _strip_prefix(text or "")
    line = _URL.sub("<url>", line)
    line = _PATH.sub("<path>", line)
    line = _RECEIPT.sub("<receipt>", line)
    line = _NUM.sub("N", line)
    line = re.sub(r"\s+", " ", line).strip()
    if len(line) > limit:
        line = line[:limit - 1] + "…"
    return line or "(no reason given)"


def reason_key(text: str | None, raw: bool = False) -> str:
    """Grouping key for a failure reason.

    Default (redacted): a KNOWN_MESSAGES id if one matches anywhere in the
    text, else the err_class token the text starts with, else
    "unrecognised". Nothing from the text itself is returned. raw=True
    returns normalize_reason(text) instead.
    """
    if raw:
        return normalize_reason(text)
    if not text or not text.strip():
        return "(no reason given)"
    for rid, pat in _KNOWN:
        if pat.search(text):
            return rid
    line = _strip_prefix(text)
    for cls in ERROR_CLASSES:
        if line.startswith(cls):
            return cls
    return "unrecognised"


def pick_sample(items: list, k: int, seed: int) -> list:
    """All items when k <= 0 or k >= len(items); otherwise a seeded sample of
    k, returned in the original order."""
    if k <= 0 or k >= len(items):
        return list(items)
    idx = sorted(random.Random(seed).sample(range(len(items)), k))
    return [items[i] for i in idx]


def is_rate_limited(text) -> bool:
    """True when a failed call's text says the serve's rate limit refused it."""
    t = (text or "").lower()
    return "429" in t or "too many requests" in t or "rate limit" in t


def receipt_outcome(stats_body, is_error: bool, text: str = "", raw: bool = False) -> dict:
    """Reduce one lambo_stats(receipt) reply to {"state", "reason"}."""
    if is_error or not isinstance(stats_body, dict):
        return {"state": "query_error", "reason": reason_key(text, raw)}
    r = stats_body.get("receipt")
    if not isinstance(r, dict):
        return {"state": "unknown", "reason": None}
    state = r.get("state") if isinstance(r.get("state"), str) else "unknown"
    reason = reason_key(r.get("detail"), raw) if state in SETTLED_FAILURE_STATES else None
    return {"state": state, "reason": reason}


def summarize_writes(acked: int, refused: list[str], outcomes: list[dict],
                     ledger_errors: list[str] | None = None,
                     transport_failed: int = 0) -> dict:
    """Aggregate write outcomes.

    acked: writes the server acknowledged with a receipt.
    refused: reason keys of writes refused at ack (tool error, no receipt).
    outcomes: receipt_outcome() dicts for the queried receipts.
    transport_failed: write calls whose reply never arrived (may or may not
    have been admitted).

    failure_rate: failed + dropped + close_unflushed over settled queried
    receipts.
    write_failure_rate_all: (failed + dropped + close_unflushed + refused +
    transport_failed)
    over (settled + refused + transport_failed). Both None when undefined.
    """
    states = collections.Counter(o["state"] for o in outcomes)
    failed = [o for o in outcomes if o["state"] in SETTLED_FAILURE_STATES]
    settled = sum(n for s, n in states.items() if s not in UNSETTLED_STATES)
    by_reason = collections.Counter(f"{o['state']}: {o['reason']}" for o in failed)
    denom_all = settled + len(refused) + transport_failed
    out = {
        "acked": acked,
        "refused_at_ack": len(refused),
        "refused_at_ack_reasons": dict(collections.Counter(refused).most_common()),
        "transport_failed": transport_failed,
        "receipts_queried": len(outcomes),
        "receipt_states": dict(states.most_common()),
        "settled": settled,
        "failed": len(failed),
        "failure_rate": round(len(failed) / settled, 4) if settled else None,
        "write_failure_rate_all": (
            round((len(failed) + len(refused) + transport_failed) / denom_all, 4)
            if denom_all else None),
        "failed_reasons": dict(by_reason.most_common()),
    }
    if ledger_errors is not None:
        out["failed_reasons_server_ledger"] = dict(
            collections.Counter(ledger_errors).most_common())
    return out


def ledger_failure_reasons(lines, receipts: set[str], raw: bool = False) -> list[str]:
    """Reason keys of each failed `completion` line whose receipt is one of
    ours. `lines` is an iterable of JSONL strings; bad lines are skipped."""
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
            out.append(reason_key(str(d.get("error") or ""), raw))
    return out


DRAIN_KEYS = ("write_queue_outstanding", "log_depth", "flush_depth")


def drain_done(stats) -> bool:
    """True only when every DRAIN_KEYS value is present, an int, and zero."""
    if not isinstance(stats, dict):
        return False
    for k in DRAIN_KEYS:
        v = stats.get(k)
        if not isinstance(v, int) or isinstance(v, bool) or v != 0:
            return False
    return True


def retention_flags(outcomes: list[dict], run_s: float, drain_s: float) -> dict:
    expired = sum(1 for o in outcomes if o["state"] == "expired")
    return {"expired_receipts": expired,
            "retention_risk": run_s + drain_s >= RETENTION_WARN_FRACTION * RECEIPT_RETENTION_S,
            "receipt_retention_s": RECEIPT_RETENTION_S}


def exit_problems(drained: bool, ok_calls: int, worker_errors: int) -> list[str]:
    """Reasons the run is not a result; empty means exit 0."""
    out = []
    if not drained:
        out.append("the write queue did not drain; counts are a snapshot, not totals")
    if ok_calls == 0:
        out.append("no call succeeded")
    if worker_errors:
        out.append(f"{worker_errors} worker(s) died on an unexpected exception")
    return out


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

OK, TOOL_ERROR, TRANSPORT = "ok", "tool_error", "transport"


class Client(mcp_ab.Mcp):
    """mcp_ab.Mcp with a per-worker agent id and a non-raising call."""

    def __init__(self, url, agent_id, token):
        self.agent_id = agent_id
        super().__init__(url, token)

    def call(self, name, **args):
        """(ms, outcome in OK/TOOL_ERROR/TRANSPORT, structured dict or None, text)."""
        args["agent_id"] = self.agent_id
        t = time.perf_counter()
        try:
            res = self._rpc("tools/call", {"name": name, "arguments": args})
        except Exception as e:  # transport or JSON-RPC error
            code = getattr(e, "code", None)  # urllib HTTPError carries the status
            detail = f" {code}" if isinstance(code, int) else ""
            return (time.perf_counter() - t) * 1000, TRANSPORT, None, f"transport: {type(e).__name__}{detail}"
        ms = (time.perf_counter() - t) * 1000
        if not isinstance(res, dict):
            return ms, TRANSPORT, None, "transport: non-object result"
        content = res.get("content") if isinstance(res.get("content"), list) else []
        text = "\n".join(str(c.get("text", "")) for c in content if isinstance(c, dict))
        body = res.get("structuredContent")
        body = body if isinstance(body, dict) else None
        return ms, TOOL_ERROR if res.get("isError") else OK, body, text


def run(o, queries, mix, token):
    lat = collections.defaultdict(list)
    err_lat = collections.defaultdict(list)
    errs = collections.Counter()
    refused: list[str] = []
    transport_failed = [0]
    receipts: list[tuple[int, str]] = []  # (worker, receipt id)
    worker_errors: list[str] = []
    lock = threading.Lock()
    stop = time.time() + o.secs
    clients = [Client(o.endpoint, f"conc-{i}", token) for i in range(o.workers)]
    tools, weights = [m[0] for m in mix], [m[1] for m in mix]
    raw = o.include_raw_reasons

    def worker(i):
        try:
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
                ms, outcome, body, text = r
                with lock:
                    if outcome == OK:
                        lat[tool].append(ms)
                    else:
                        errs[tool] += 1
                        err_lat[tool].append(ms)
                    if tool in WRITE_TOOLS:
                        rid = (body or {}).get("receipt") if outcome == OK else None
                        if isinstance(rid, str):
                            receipts.append((i, rid))
                        elif outcome == TRANSPORT:
                            transport_failed[0] += 1
                        else:
                            refused.append(reason_key(text, raw))
        except Exception as e:  # a bug or an unexpected reply shape: count it, fail the run
            with lock:
                worker_errors.append(f"worker {i}: {type(e).__name__}")

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
        _, outcome, body, _ = warm.call("lambo_stats")
        if outcome == OK and body is not None:
            st = body
        if drain_done(st):
            drained = True
            break
        if time.time() - drain_t > o.drain_timeout:
            break
        time.sleep(0.5)
    drain_s = time.time() - drain_t

    sample = pick_sample(receipts, o.receipt_max, o.seed)
    outcomes = []
    # The serve rate-limits each credential (DEFAULT_RATE_LIMIT_RPS, 50/s with a
    # 2x burst), and every client here shares one. Unpaced, the queries after
    # the burst come back 429 and would be counted as query_error. Pace under
    # the limit and retry a rate-limited query with backoff.
    interval = 1.0 / o.receipt_rps if o.receipt_rps > 0 else 0.0
    for wi, rid in sample:
        attempts = 6
        for attempt in range(attempts):
            _, outcome, body, text = clients[wi].call("lambo_stats", receipt=rid, wait_ms=0)
            if outcome == OK or not is_rate_limited(text) or attempt == attempts - 1:
                break
            time.sleep(min(0.25 * 2 ** attempt, 4.0))
        outcomes.append(receipt_outcome(body, outcome != OK, text, raw))
        if interval:
            time.sleep(interval)

    ledger_errors = None
    if o.server_ledger:
        mine = {rid for _, rid in receipts}
        with open(o.server_ledger, errors="replace") as f:
            ledger_errors = ledger_failure_reasons(f, mine, raw)

    keep = {k: st.get(k) for k in st
            if k.startswith("write_queue_") or k in (
                "dead_lettered", "degraded", "concept_count", "embedded_concepts")}
    writes = summarize_writes(len(receipts), refused, outcomes, ledger_errors,
                              transport_failed[0])
    writes["sampled"] = len(sample) < len(receipts)
    ok_calls = sum(len(v) for v in lat.values())
    return {
        "workers": o.workers, "secs": o.secs, "think": o.think, "seed": o.seed,
        "mix": {t: w for t, w in mix},
        "wall_s": round(wall, 1),
        "drain": {"drained": drained, "drain_s": round(drain_s, 1),
                  "drain_timeout_s": o.drain_timeout},
        "calls": ok_calls + sum(errs.values()),
        "ok_calls": ok_calls,
        "latency_ms": {k: mcp_ab.summ(v) for k, v in sorted(lat.items())},
        "errors": {k: {"count": errs[k], "latency_ms": mcp_ab.summ(err_lat[k])}
                   for k in sorted(errs)},
        "worker_errors": len(worker_errors),
        "worker_error_kinds": dict(collections.Counter(
            e.split(": ", 1)[1] for e in worker_errors)),
        "writes": writes,
        "flags": retention_flags(outcomes, wall, drain_s),
        "reasons_redacted": not raw,
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
    ap.add_argument("--receipt-rps", type=float, default=40.0,
                    help="pace receipt queries under the serve's per-credential rate "
                         "limit (default 40/s; 0 = unpaced). Rate-limited queries are "
                         "retried with backoff either way")
    ap.add_argument("--server-ledger",
                    help="the serve's --ledger file, read only, to group the raw "
                         "operator-facing error of each failed write of this run")
    ap.add_argument("--include-raw-reasons", action="store_true",
                    help="group reasons by normalized server text instead of known-message "
                         "ids and error classes (the text can echo server detail; do not "
                         "paste into a public issue unread)")
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
    problems = exit_problems(res["drain"]["drained"], res["ok_calls"], res["worker_errors"])
    res["problems"] = problems
    with open(o.out, "w") as f:
        json.dump(res, f, indent=2)
    print(json.dumps(res, indent=2))
    fl = res["flags"]
    if fl["expired_receipts"] or fl["retention_risk"]:
        print(f"warning: {fl['expired_receipts']} receipt(s) expired; run + drain was "
              f"{res['wall_s'] + res['drain']['drain_s']:.0f}s against a "
              f"{RECEIPT_RETENTION_S:.0f}s receipt retention", file=sys.stderr)
    for p in problems:
        print(f"error: {p}", file=sys.stderr)
    return EXIT_INCOMPLETE if problems else 0


if __name__ == "__main__":
    sys.exit(main())
