#!/usr/bin/env python3
"""F4 burst driver: fire N at-cap record_action writes as fast as round-trips
allow against a `lambo serve --transport http` endpoint whose SIGTERM is pulled
by the calling harness as soon as this prints BURST_DONE.

Each call: one MCP write (one durable write intent + ~64 fresh concept
mutations). Every concept name is unique per (worker, seq), so every call is a
genuinely new write — no canonization dedup collapses it into an existing node.

Ledger: one JSON line per call with the ack receipt id and elapsed ms.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request

ENDPOINT = "http://127.0.0.1:7700/mcp"
PROTOCOL_VERSION = "2025-06-18"
MAX_ACTION_TARGETS = 64


class Mcp:
    def __init__(self, endpoint: str, token: str | None):
        self.endpoint = endpoint
        self.session: str | None = None
        self.token = token
        self._id = 0

    def _headers(self) -> dict[str, str]:
        h = {"Content-Type": "application/json",
             "Accept": "application/json, text/event-stream"}
        if self.session:
            h["Mcp-Session-Id"] = self.session
        if self.token:
            h["Authorization"] = f"Bearer {self.token}"
        return h

    def _post(self, payload: dict, expect_reply: bool = True):
        req = urllib.request.Request(self.endpoint,
                                     data=json.dumps(payload).encode(),
                                     headers=self._headers(), method="POST")
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
            for raw in r:
                line = raw.decode().strip()
                if not line.startswith("data:"):
                    continue
                chunk = line[5:].strip()
                if chunk.startswith("{"):
                    return status, json.loads(chunk)
        raise RuntimeError("MCP stream ended before a reply arrived")

    def initialize(self) -> dict:
        status, reply = self._post({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": PROTOCOL_VERSION,
                       "capabilities": {},
                       "clientInfo": {"name": "f4-cockroach", "version": "1"}}})
        if reply is None or "error" in reply:
            raise RuntimeError(f"initialize: {reply}")
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"},
                   expect_reply=False)
        return reply.get("result", {})

    def call(self, method: str, params: dict):
        self._id += 1
        status, reply = self._post(
            {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params})
        if reply is None:
            raise RuntimeError(f"{method}: empty reply")
        return status, reply


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, required=True, help="number of record_action writes")
    ap.add_argument("--endpoint", default=ENDPOINT)
    ap.add_argument("--token", default=None)
    ap.add_argument("--agent", default="f4-agent")
    ap.add_argument("--ledger", required=True)
    ap.add_argument("--k-component", "--per", dest="per", type=int, default=16,
                    help="produces per call (targets split across the three lists, <=64)")
    args = ap.parse_args()
    args.token = args.token or os.environ.get("LAMBO_AUTH_TOKEN")

    mcp = Mcp(args.endpoint, args.token)
    mcp.initialize()

    # Tune target split so total <= 64 (the server's at-cap bound).
    n = args.per
    produces = n // 2
    modifies = n // 4
    depends = n - produces - modifies

    out = open(args.ledger, "a", encoding="utf-8")
    wrote = 0
    t0 = time.time()
    for seq in range(1, args.n + 1):
        names = {"p": [], "m": [], "d": []}
        plist = [f"f4-{seq}-p{i}" for i in range(produces)]
        mlist = [f"f4-{seq}-m{i}" for i in range(modifies)]
        dlist = [f"f4-{seq}-d{i}" for i in range(depends)]
        params = {
            "agent_id": args.agent,
            "action": f"f4 burst action {seq}",
            "produces": plist,
            "modifies": mlist,
            "depends_on": dlist,
        }
        started = time.perf_counter()
        status, reply = mcp.call("tools/call", {
            "name": "lambo_record_action", "arguments": params})
        elapsed_ms = int((time.perf_counter() - started) * 1000)
        result = reply.get("result", {})
        sc = result.get("structuredContent") or {}
        receipt = sc.get("receipt")
        admitted = sc.get("receipt_state")
        is_error = bool(result.get("isError"))
        text = " ".join(c.get("text", "") for c in result.get("content", [])
                        if c.get("type") == "text")
        out.write(json.dumps({
            "seq": seq, "receipt": receipt, "receipt_state": admitted,
            "is_error": is_error, "elapsed_ms": elapsed_ms,
            "t": time.time(), "text": text[:200],
        }, ensure_ascii=True) + "\n")
        out.flush()
        wrote += 1
        if is_error:
            print(f"seq {seq} ERROR: {text[:200]}", file=sys.stderr)
    out.close()
    print(f"BURST_DONE n={args.n} wrote={wrote} elapsed={time.time()-t0:.2f}s",
          flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
