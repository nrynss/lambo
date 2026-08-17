#!/usr/bin/env python3
"""Concurrency-capture load driver for `lambo serve` over streamable HTTP MCP.

C1 of the concurrency capture (dev-diary/notes/concurrency-capture.md). Drives
`lambo serve --transport http` with K concurrent clients issuing a weighted mix
of valid and adversarial tool calls, and records EVERY response to a JSONL
ledger. The ledger is the ground truth for the C2 SIGTERM capture and the C3
durability comparison.

    python3 scripts/loadtest/mcp_load.py \
        --session c-load-20260818 --ledger /tmp/load-ledger.jsonl \
        --workers 12 --token "$SCRATCH_TOKEN"

Stdlib only (urllib), mirroring the streamable-HTTP MCP client in
examples/drive_mcp_soak.py: initialize -> notifications/initialized ->
tools/call, `Mcp-Session-Id` header, accepting both JSON and SSE replies.

The HTTP surface enforces documented limits (T8.7) — a sustained rate limit of
`DEFAULT_RATE_LIMIT_RPS` (50, burst x2) and a session cap of
`DEFAULT_MAX_SESSIONS` (32). A refusal from either is a *correct* observation,
not a failure. The driver shapes pacing so those refusals never crowd out the
measurement: the main window paces below the limit, the cap probe observes 503
against the session ceiling, and the burst phase free-runs briefly (429s) before
settling to a paced tail-building rate for the SIGTERM capture.

Phases, in order (all written to the ledger as `phase` records):

1. sessions   — every worker opens its own MCP session (initialize + notified).
2. cap-probe  — a probe mints sessions until the server refuses with 503, then
                releases them with DELETE /mcp. Proves the session cap is live.
3. overdrive  — workers free-run against the fresh, fast server (bounded to
                `--overdrive-calls` each), so the rate limit's 429s are
                genuinely observed without flooding it.
4. main       — paced valid+adversarial mix at `--rate` rps aggregate.
5. burst      — at-cap record_action calls paced at `--burst-rate`, building a
                large un-flushed tail. The harness sends SIGTERM here.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
import threading
import time
import urllib.error
import urllib.request

DEFAULT_ENDPOINT = "http://127.0.0.1:7700/mcp"
PROTOCOL_VERSION = "2025-06-18"

# Cap numbers mirrored from the server (`src/cli/caps.rs`, `src/mcp/serve.rs`)
# so the adversarial mix targets the real bounds.
MAX_ACTION_TARGETS = 64
MAX_CONTENT_BYTES = 16_384

# Concept vocabulary. A shared pool (workers re-derive the same contents, so
# derives often MATCH existing nodes — the canonization dedup behaviour C5
# measures) plus a per-worker deterministic tail of fresh contents.
SHARED_POOL = [
    ("user schema", "entity"),
    ("auth middleware", "entity"),
    ("billing retries change", "constraint"),
    ("must stay backward compatible", "constraint"),
    ("session store", "entity"),
    ("cache layer", "entity"),
    ("shared config", "entity"),
    ("migrations dir", "resource"),
    ("rate limit bucket", "logic"),
    ("VPC-Enterprise-Prod", "entity"),
    ("Subnet-Public-1a", "entity"),
    ("InternetGateway", "entity"),
    ("RouteTable-Public", "entity"),
    ("SG-Base-VPC", "entity"),
    ("SG-PublicWeb", "entity"),
    ("RDS-Lambo-Demo-DB", "entity"),
    ("Lambda-LamboStats-API", "entity"),
    ("EC2-LamboWebExhibit", "entity"),
]
PARENT_OF_POOL = [
    ("VPC-Enterprise-Prod", "Subnet-Public-1a"),
    ("VPC-Enterprise-Prod", "InternetGateway"),
    ("VPC-Enterprise-Prod", "RouteTable-Public"),
    ("SG-Base-VPC", "SG-PublicWeb"),
    ("Subnet-Public-1a", "EC2-LamboWebExhibit"),
    ("auth middleware", "user schema"),
    ("session store", "user schema"),
]
RECALL_QUERIES = [
    "update user schema",
    "which component retries billing payments",
    "what depends on SG-Base-VPC",
    "cache layer behaviour",
    "session store migration",
]
ACTIONS = [
    "created migrations/003.sql",
    "refactored the billing retry loop",
    "added the rate limit middleware",
    "updated the schema migration",
    "deployed the exhibit binary",
]


class Ledger:
    """Thread-safe append-only JSONL writer. One call (one line) at a time."""

    def __init__(self, path: str) -> None:
        self._lock = threading.Lock()
        self._fh = open(path, "a", encoding="utf-8")

    def write(self, record: dict) -> None:
        # ensure_ascii=True keeps the adversarial content (NUL, U+202E) as
        # visible escapes — faithful, but greppable and isutf8-clean.
        line = json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n"
        with self._lock:
            self._fh.write(line)
            self._fh.flush()

    def close(self) -> None:
        with self._lock:
            self._fh.close()


class Mcp:
    """Minimal streamable-HTTP MCP client. Enough for initialize + tools/call."""

    def __init__(self, endpoint: str, token: str | None) -> None:
        self.endpoint = endpoint
        self.session: str | None = None
        self.token = token
        self._id = 0

    def _headers(self) -> dict[str, str]:
        headers = {
            "Content-Type": "application/json",
            # The server may answer either way; accept both rather than assume.
            "Accept": "application/json, text/event-stream",
        }
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        return headers

    def _post(self, payload: dict, expect_reply: bool = True) -> tuple[int, dict | None]:
        req = urllib.request.Request(
            self.endpoint,
            data=json.dumps(payload).encode(),
            headers=self._headers(),
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=120) as r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session = sid
            status = r.status
            if not expect_reply:
                return status, None
            if "text/event-stream" not in (r.headers.get("Content-Type") or ""):
                body = r.read().decode().strip()
                return status, (json.loads(body) if body else None)
            # The stream stays open after the reply, so read line by line and
            # stop at the first JSON frame rather than waiting for a close that
            # only comes when the session ends.
            for raw in r:
                line = raw.decode().strip()
                if not line.startswith("data:"):
                    continue
                chunk = line[5:].strip()
                if chunk.startswith("{"):
                    return status, json.loads(chunk)
        raise RuntimeError("MCP stream ended before a reply arrived")

    def initialize(self) -> dict:
        status, reply = self._post(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "c-series-mcp-load", "version": "1"},
                },
            }
        )
        assert reply is not None, "empty initialize reply"
        if "error" in reply:
            raise RuntimeError(f"initialize: {reply['error']}")
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"}, expect_reply=False)
        return reply.get("result", {})

    def call(self, method: str, params: dict | None = None) -> tuple[int, dict]:
        self._id += 1
        status, reply = self._post(
            {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}}
        )
        if reply is None:
            raise RuntimeError(f"{method}: empty reply")
        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error']}")
        return status, reply.get("result", {})

    def close_session(self) -> None:
        """DELETE /mcp — release the session so the server's live count drops."""
        if not self.session:
            return
        req = urllib.request.Request(self.endpoint, headers=self._headers(), method="DELETE")
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                r.read()
        except Exception:
            pass  # release is best-effort; the session dies with the process anyway
        self.session = None


def tool_text(result: dict) -> str:
    parts = [c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"]
    return " ".join(p.strip() for p in parts if p).strip()


class Worker:
    """One MCP client issuing calls from the weighted mix until told to stop."""

    def __init__(
        self,
        idx: int,
        ledger: Ledger,
        rng: random.Random,
        args: argparse.Namespace,
        run: threading.Event,
    ) -> None:
        self.idx = idx
        self.ledger = ledger
        self.rng = rng
        self.args = args
        self.run = run
        self.session_id: str | None = None
        self.ready = threading.Event()
        self.phase = "main"  # swapped by the controller under the phase lock
        self.phase_lock = threading.Lock()
        self.agent = f"{args.agent_prefix}-{idx}"
        self._seq = 0

    def set_phase(self, phase: str) -> None:
        with self.phase_lock:
            self.phase = phase

    def get_phase(self) -> str:
        with self.phase_lock:
            return self.phase

    def open_session(self) -> None:
        """Open this worker's MCP session. Raises on failure so main() aborts
        the run: a worker without a session would otherwise spin on transport
        errors and pollute the ledger with noise instead of measurement."""
        mcp = Mcp(self.args.endpoint, self.args.token)
        info = mcp.initialize()
        self.session_id = mcp.session
        self.mcp = mcp
        self.ledger.write(
            {
                "kind": "session",
                "worker": self.idx,
                "agent": self.agent,
                "session_id": self.session_id,
                "server": f"{info.get('serverInfo', {}).get('name', '?')} "
                f"{info.get('serverInfo', {}).get('version', '')}".rstrip(),
                "t": time.time(),
            }
        )
        self.ready.set()

    def _pick_valid(self) -> tuple[str, dict]:
        """A valid tool call: derive / record_action / recall.

        During the burst phase the mix collapses to at-cap `record_action`
        calls — the burst exists to build a large un-flushed mutation tail for
        the SIGTERM capture, and each at-cap call is ~128 mutations.
        """
        agent = self.agent
        if self.get_phase() == "burst":
            n = MAX_ACTION_TARGETS
            produces = [f"burst-{self.idx}-p{i}" for i in range(n // 2)]
            modifies = [f"burst-{self.idx}-m{i}" for i in range(n // 4)]
            depends_on = [f"burst-{self.idx}-d{i}" for i in range(n - n // 2 - n // 4)]
            return "lambo_record_action", {
                "agent_id": agent,
                "action": f"burst action {self.idx}-{self._seq}",
                "produces": produces,
                "modifies": modifies,
                "depends_on": depends_on,
            }
        roll = self.rng.random()
        if roll < 0.38:
            # derive: a few concepts, some with parent_of
            n = self.rng.randint(1, 4)
            concepts = []
            for _ in range(n):
                if self.rng.random() < 0.5:
                    content, ctype = self.rng.choice(SHARED_POOL)
                else:
                    content = f"load concept {self.idx}-{self._seq}-{self.rng.randint(0, 9999)}"
                    ctype = self.rng.choice(["entity", "logic", "constraint", "resource"])
                concepts.append({"content": content, "concept_type": ctype})
            params: dict = {"agent_id": agent, "concepts": concepts}
            if self.rng.random() < 0.3:
                params["parent_of"] = [
                    {"parent": p, "child": c} for p, c in self.rng.sample(PARENT_OF_POOL, 1)
                ]
            return "lambo_derive", params
        if roll < 0.63:
            # record_action: a handful of targets (well under the cap)
            produces = [f"{self.agent}-p{i}" for i in range(self.rng.randint(1, 4))]
            modifies = self.rng.sample([c for c, _ in SHARED_POOL], self.rng.randint(0, 2))
            depends_on = self.rng.sample([c for c, _ in SHARED_POOL], self.rng.randint(1, 2))
            return "lambo_record_action", {
                "agent_id": agent,
                "action": self.rng.choice(ACTIONS),
                "produces": produces,
                "modifies": modifies,
                "depends_on": depends_on,
            }
        return "lambo_recall", {
            "agent_id": agent,
            "query": self.rng.choice(RECALL_QUERIES),
        }

    def _pick_adversarial(self) -> tuple[str, dict, str]:
        """An adversarial call: refused at the wire, the schema, or the caps."""
        agent = self.agent
        pattern = self.rng.choice(
            [
                "over-targets",
                "nul-content",
                "rtl-content",
                "over-content",
                "unknown-tool",
                "malformed-params",
            ]
        )
        if pattern == "over-targets":
            # One over the cap, split across the three lists (N1's shape).
            n = MAX_ACTION_TARGETS + 1
            produces = [f"over-p{i}" for i in range(max(1, n // 3))]
            modifies = [f"over-m{i}" for i in range(max(1, n // 3))]
            depends_on = [f"over-d{i}" for i in range(n - len(produces) - len(modifies))]
            return "lambo_record_action", {
                "agent_id": agent,
                "action": "touch everything",
                "produces": produces,
                "modifies": modifies,
                "depends_on": depends_on,
            }, pattern
        if pattern == "nul-content":
            return "lambo_derive", {
                "agent_id": agent,
                "concepts": [{"content": "user schema\x00backdoor", "concept_type": "entity"}],
            }, pattern
        if pattern == "rtl-content":
            return "lambo_derive", {
                "agent_id": agent,
                "concepts": [{"content": "user\u202Eschema", "concept_type": "entity"}],
            }, pattern
        if pattern == "over-content":
            big = "A" * (MAX_CONTENT_BYTES + 1)
            return "lambo_record_action", {
                "agent_id": agent,
                "action": big,
            }, pattern
        if pattern == "unknown-tool":
            return "lambo_frobnicate", {
                "agent_id": agent,
                "frobnicate": True,
            }, pattern
        # malformed-params: rotate a few shapes
        shape = self.rng.choice(["missing-agent", "missing-concepts", "wrong-type", "bad-pair"])
        if shape == "missing-agent":
            return "lambo_derive", {
                "concepts": [{"content": "user schema", "concept_type": "entity"}]
            }, pattern
        if shape == "missing-concepts":
            return "lambo_derive", {"agent_id": agent}, pattern
        if shape == "wrong-type":
            return "lambo_recall", {"agent_id": agent, "query": 42}, pattern
        return "lambo_derive", {
            "agent_id": agent,
            "concepts": [{"content": "user schema", "concept_type": "entity"}],
            "parent_of": [{"parent": "user schema"}],  # missing child
        }, pattern

    def _one_call(self) -> dict:
        """Make one call from the mix and return its ledger record."""
        is_adversarial = self.rng.random() < self.args.adversarial_fraction
        if is_adversarial:
            tool, params, pattern = self._pick_adversarial()
        else:
            tool, params = self._pick_valid()
            pattern = "valid"
        self._seq += 1
        started = time.perf_counter()
        status, result = None, None
        ok, is_error, error, text = False, None, None, ""
        try:
            # tools/call straight on the wire, so a JSON-RPC error (e.g. an
            # unknown tool) is recorded as what it is: a tool-level error with
            # its HTTP status, not a transport failure.
            status, reply = self.mcp._post(
                {
                    "jsonrpc": "2.0",
                    "id": self._seq,
                    "method": "tools/call",
                    "params": {"name": tool, "arguments": params},
                }
            )
            if reply is None:
                raise RuntimeError("empty reply")
            if "error" in reply:
                ok = True
                is_error = True
                error = json.dumps(reply["error"], ensure_ascii=True)[:4000]
            else:
                result = reply.get("result", {})
                ok = True
                is_error = bool(result.get("isError"))
                text = tool_text(result)
                if is_error:
                    error = text[:4000]
        except urllib.error.HTTPError as e:
            status = e.code
            try:
                error = e.read().decode().strip()[:4000]
            except Exception:
                error = str(e)
        except Exception as e:  # transport error (server died mid-burst, etc.)
            error = f"{type(e).__name__}: {e}"[:4000]
        elapsed_ms = int((time.perf_counter() - started) * 1000)
        return {
            "kind": "call",
            "worker": self.idx,
            "agent": self.agent,
            "seq": self._seq,
            "tool": tool,
            "pattern": pattern,
            "params": params,
            "ok": ok,
            "is_error": is_error,
            "error": error,
            "text": text[:4000],
            "http_status": status,
            "elapsed_ms": elapsed_ms,
            "t": time.time(),
        }

    def run_loop(self, main_rate: float, burst_rate: float) -> None:
        """Emit calls until the run event clears.

        Pacing by phase:
        * `overdrive` — free-run against the fresh server, bounded per worker
          by `--overdrive-calls`, so the rate limit's 429s are genuinely
          observed without flooding; then idle until the controller moves on.
        * `main` — `main_rate` per worker, valid + adversarial mix.
        * `burst` — `burst_rate` per worker, at-cap record_action calls, so a
          large un-flushed tail keeps building for the SIGTERM capture.
        Ten consecutive transport failures mean the server is gone (the
        harness SIGTERMed it); stop cleanly and let the ledger show the seam
        instead of hammering a dead socket.
        """
        overdrive_left = self.args.overdrive_calls
        dead = 0
        while self.run.is_set():
            rec = self._one_call()
            self.ledger.write(rec)
            if rec["ok"]:
                dead = 0
            elif rec.get("http_status") is None:
                dead += 1
                if dead >= 10:
                    self.ledger.write(
                        {
                            "kind": "phase",
                            "name": "server-unreachable",
                            "worker": self.idx,
                            "t": time.time(),
                        }
                    )
                    return
            phase = self.get_phase()
            if phase == "overdrive":
                if overdrive_left > 0:
                    overdrive_left -= 1
                    continue
                time.sleep(0.05)
                continue
            if phase == "burst":
                rate = burst_rate
            else:
                rate = main_rate
            wait = 1.0 / rate - rec["elapsed_ms"] / 1000.0
            if wait > 0:
                time.sleep(wait)


def cap_probe(ledger: Ledger, args: argparse.Namespace, workers_done: threading.Event) -> None:
    """Mint sessions until the server refuses with 503, then release them."""
    workers_done.wait()
    ledger.write({"kind": "phase", "name": "cap-probe-start", "t": time.time()})
    opened: list[Mcp] = []
    attempts = 0
    while attempts < args.cap_probe_max:
        attempts += 1
        mcp = Mcp(args.endpoint, args.token)
        started = time.perf_counter()
        try:
            mcp.initialize()
            opened.append(mcp)
            rec = {
                "kind": "cap_probe",
                "attempt": attempts,
                "ok": True,
                "http_status": 200,
                "session_id": mcp.session,
                "elapsed_ms": int((time.perf_counter() - started) * 1000),
                "t": time.time(),
            }
        except urllib.error.HTTPError as e:
            rec = {
                "kind": "cap_probe",
                "attempt": attempts,
                "ok": False,
                "http_status": e.code,
                "error": e.read().decode().strip()[:2000] if hasattr(e, "read") else str(e),
                "elapsed_ms": int((time.perf_counter() - started) * 1000),
                "t": time.time(),
            }
            if e.code == 503:
                ledger.write(rec)
                break
        except Exception as e:
            rec = {
                "kind": "cap_probe",
                "attempt": attempts,
                "ok": False,
                "http_status": None,
                "error": f"{type(e).__name__}: {e}"[:2000],
                "elapsed_ms": int((time.perf_counter() - started) * 1000),
                "t": time.time(),
            }
        ledger.write(rec)
    for m in opened:
        m.close_session()
    ledger.write(
        {
            "kind": "phase",
            "name": "cap-probe-end",
            "opened": len(opened),
            "attempts": attempts,
            "t": time.time(),
        }
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--session", required=True, help="scratch session id (for reporting)")
    ap.add_argument("--token", default=None, help="bearer token (default: $LAMBO_AUTH_TOKEN)")
    ap.add_argument("--ledger", required=True, help="JSONL ledger path")
    ap.add_argument("--seed", type=int, default=0, help="deterministic RNG seed")
    ap.add_argument("--workers", type=int, default=12, help="K concurrent clients (>=12)")
    ap.add_argument("--rate", type=float, default=40.0, help="main-window aggregate rps")
    ap.add_argument("--agent-prefix", default="c-load")
    ap.add_argument("--burst-rate", type=float, default=45.0, help="burst pacing aggregate rps")
    ap.add_argument("--overdrive", type=float, default=2.0, help="burst free-run seconds (429s)")
    ap.add_argument("--overdrive-calls", type=int, default=120, help="max free-run calls per worker")
    ap.add_argument("--main-secs", type=float, default=45.0)
    ap.add_argument("--burst-secs", type=float, default=20.0)
    ap.add_argument("--adversarial-fraction", type=float, default=0.2)
    ap.add_argument("--cap-probe-max", type=int, default=64)
    args = ap.parse_args()
    args.token = args.token or os.environ.get("LAMBO_AUTH_TOKEN")

    ledger = Ledger(args.ledger)
    ledger.write(
        {
            "kind": "meta",
            "run_id": time.strftime("%Y%m%d-%H%M%S"),
            "session": args.session,
            "seed": args.seed,
            "workers": args.workers,
            "rate": args.rate,
            "burst_rate": args.burst_rate,
            "overdrive": args.overdrive,
            "adversarial_fraction": args.adversarial_fraction,
            "main_secs": args.main_secs,
            "burst_secs": args.burst_secs,
            "started_at": time.time(),
        }
    )

    run = threading.Event()
    run.set()

    workers = [
        Worker(i, ledger, random.Random(args.seed * 1000 + i), args, run)
        for i in range(args.workers)
    ]
    threads = []
    for w in workers:
        t = threading.Thread(target=w.open_session, name=f"open-{w.idx}", daemon=True)
        t.start()
        threads.append(t)
    failures: list[str] = []
    for t in threads:
        t.join()
    for w in workers:
        # open_session sets `ready` only on success; a worker that died opening
        # its session would otherwise spin on transport errors and pollute the
        # ledger with noise instead of measurement, so abort the run.
        if not w.ready.is_set():
            failures.append(f"worker {w.idx} could not open its MCP session")
    if failures:
        print("aborting: " + "; ".join(failures), file=sys.stderr)
        ledger.close()
        return 2

    # Cap probe runs after all workers hold their sessions (they were joined
    # above, so the event is set before the probe thread can wait on it).
    probe_done = threading.Event()
    probe_done.set()
    probe = threading.Thread(
        target=cap_probe, args=(ledger, args, probe_done), name="cap-probe", daemon=True
    )
    probe.start()
    probe.join()

    # Overdrive: free-run against the fresh, fast server so the rate limit's
    # 429s are genuinely observed (on a loaded server the bottleneck would be
    # the server itself, and the limiter would never fire — the first capture
    # proved that the hard way).
    main_rate = args.rate / args.workers
    burst_rate = args.burst_rate / args.workers
    ledger.write({"kind": "phase", "name": "overdrive-start", "t": time.time()})
    for w in workers:
        w.set_phase("overdrive")
    for w in workers:
        t = threading.Thread(
            target=w.run_loop, args=(main_rate, burst_rate), name=f"worker-{w.idx}", daemon=True
        )
        t.start()
    time.sleep(max(0.0, args.overdrive))
    ledger.write({"kind": "phase", "name": "overdrive-end", "t": time.time()})

    # Main window: paced valid + adversarial mix.
    ledger.write({"kind": "phase", "name": "main-start", "t": time.time()})
    main_deadline = time.time() + args.main_secs
    for w in workers:
        w.set_phase("main")
    time.sleep(max(0.0, main_deadline - time.time()))
    ledger.write({"kind": "phase", "name": "main-end", "t": time.time()})


    # Burst: paced at-cap record_actions building the un-flushed tail. The
    # harness sends SIGTERM during this phase.
    ledger.write({"kind": "phase", "name": "burst-start", "t": time.time()})
    burst_deadline = time.time() + args.burst_secs
    for w in workers:
        w.set_phase("burst")
    time.sleep(max(0.0, burst_deadline - time.time()))
    ledger.write({"kind": "phase", "name": "burst-end", "t": time.time()})

    run.clear()
    time.sleep(1.0)  # let in-flight calls land their records
    ledger.write({"kind": "phase", "name": "done", "t": time.time()})
    ledger.close()

    # Summary (stderr; the ledger is the evidence).
    total = ok_count = 0
    kinds: dict[str, list[int]] = {}
    for line in open(args.ledger, encoding="utf-8"):
        rec = json.loads(line)
        if rec["kind"] != "call":
            continue
        total += 1
        if rec["ok"] and not rec["is_error"]:
            ok_count += 1
        kinds.setdefault(rec["pattern"], [0, 0])
        kinds[rec["pattern"]][0] += 1
        if rec["ok"] and not rec["is_error"]:
            kinds[rec["pattern"]][1] += 1
    print(
        f"ledger: {total} calls, {ok_count} ok — "
        + ", ".join(f"{k}={v[1]}/{v[0]}" for k, v in sorted(kinds.items())),
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
