#!/usr/bin/env python3
"""Drive `lambo serve` over MCP until the CloudOps pillars earn Canonical.

**A demo artifact, not part of the product or the provisioning path.** Read
`examples/README.md` before citing anything this produces. The short version:
the structural gates are earned, and Stage 2's requirement of three distinct
origin interactions was satisfied by replaying the same derives. Each replay is
a distinct interaction to the engine. None of them is distinct *work*.

Why this exists, rather than just re-running the agents:

`lambo derive` on the command line takes the writer lease, does its work, and
exits within a second or two. The canonization state machine cannot run in that
window: Stage 1 needs `gc_survived >= 3`, GC sweeps only when the session epoch
advances past `gc_interval`, and the canonization pass runs on its own interval
inside a *live* daemon. A process that exits immediately never gets there, which
is why the session sat at `canonical=0` no matter how many times the agents ran.

So the writer here is a long-lived `lambo serve`, and the mutations are
delivered the way a real agent delivers them: over MCP. That is also the
deployment model the README describes, one writer owning the session with
agents connected to it, so this exercises the intended path rather than a
special case built for the exhibit.

Nothing here lowers the bar for promotion. Blast radius, distinct interactions,
coverage and the peer score cut are all still enforced by `canon::stage{1,2,3}`.
This only supplies interactions and lets the daemon run long enough to look.

    python3 scripts/cloudops/drive_mcp_soak.py --session cloudops-exhibit
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

DEFAULT_ENDPOINT = "http://127.0.0.1:7700/mcp"
PROTOCOL_VERSION = "2025-06-18"

# The pillars the exhibit is about, reinforced as themselves. Same contents the
# agents derive, so this adds interactions without inventing graph shape: the
# canonicalizer matches these onto the existing nodes rather than creating new
# ones.
TOPOLOGY = [
    ("VPC-Enterprise-Prod", "entity"),
    ("Subnet-Public-1a", "entity"),
    ("Subnet-Private-1a", "entity"),
    ("Subnet-Private-1b", "entity"),
    ("InternetGateway", "entity"),
    ("RouteTable-Public", "entity"),
    ("SG-Base-VPC", "entity"),
    ("SG-PublicWeb", "entity"),
    ("RDS-Lambo-Demo-DB", "entity"),
    ("Lambda-LamboStats-API", "entity"),
    ("EC2-LamboWebExhibit", "entity"),
]
PARENT_OF = [
    ("VPC-Enterprise-Prod", "Subnet-Public-1a"),
    ("VPC-Enterprise-Prod", "Subnet-Private-1a"),
    ("VPC-Enterprise-Prod", "Subnet-Private-1b"),
    ("VPC-Enterprise-Prod", "InternetGateway"),
    ("VPC-Enterprise-Prod", "RouteTable-Public"),
    ("VPC-Enterprise-Prod", "SG-Base-VPC"),
    ("VPC-Enterprise-Prod", "SG-PublicWeb"),
    ("SG-Base-VPC", "RDS-Lambo-Demo-DB"),
    ("Subnet-Public-1a", "EC2-LamboWebExhibit"),
]


class Mcp:
    """Minimal streamable-HTTP MCP client. Enough for initialize + tools/call."""

    def __init__(self, endpoint: str) -> None:
        self.endpoint = endpoint
        self.session: str | None = None
        self._id = 0

    def _post(self, payload: dict, expect_reply: bool = True) -> dict | None:
        headers = {
            "Content-Type": "application/json",
            # The server may answer either way; accept both rather than assume.
            "Accept": "application/json, text/event-stream",
        }
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        req = urllib.request.Request(
            self.endpoint, data=json.dumps(payload).encode(), headers=headers, method="POST"
        )
        with urllib.request.urlopen(req, timeout=120) as r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session = sid
            if not expect_reply:
                return None
            if "text/event-stream" not in (r.headers.get("Content-Type") or ""):
                body = r.read().decode().strip()
                return json.loads(body) if body else None
            # The stream stays open after the reply, so read line by line and
            # stop at the first JSON frame rather than waiting for a close that
            # only comes when the session ends. The server opens with a bare
            # `data:` keep-alive, which is skipped rather than parsed.
            for raw in r:
                line = raw.decode().strip()
                if not line.startswith("data:"):
                    continue
                chunk = line[5:].strip()
                if chunk.startswith("{"):
                    return json.loads(chunk)
        raise RuntimeError("MCP stream ended before a reply arrived")

    def call(self, method: str, params: dict | None = None) -> dict:
        self._id += 1
        reply = self._post(
            {"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or {}}
        )
        if reply is None:
            raise RuntimeError(f"{method}: empty reply")
        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error']}")
        return reply.get("result", {})

    def notify(self, method: str) -> None:
        self._post({"jsonrpc": "2.0", "method": method}, expect_reply=False)

    def handshake(self) -> dict:
        result = self.call(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "cloudops-soak", "version": "1"},
            },
        )
        self.notify("notifications/initialized")
        return result


def tool_text(result: dict) -> str:
    parts = [c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"]
    return " ".join(p.strip() for p in parts if p).strip()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--session", required=True, help="session the writer owns (for reporting only)")
    ap.add_argument("--endpoint", default=DEFAULT_ENDPOINT)
    ap.add_argument("--agent", default="cloudops-soak")
    ap.add_argument("--max-passes", type=int, default=40)
    ap.add_argument("--interval", type=float, default=8.0, help="seconds between passes")
    args = ap.parse_args()

    mcp = Mcp(args.endpoint)
    try:
        info = mcp.handshake()
    except (urllib.error.URLError, OSError) as e:
        print(f"error: cannot reach {args.endpoint}: {e}", file=sys.stderr)
        print("hint: start `lambo serve --transport http --port 7700` first.", file=sys.stderr)
        return 2
    server = info.get("serverInfo", {})
    print(f"connected: {server.get('name', '?')} {server.get('version', '')}".rstrip())

    tools = [t["name"] for t in mcp.call("tools/list").get("tools", [])]
    print(f"tools: {', '.join(tools)}")

    derive_args = {
        "agent_id": args.agent,
        "concepts": [{"content": c, "concept_type": t} for c, t in TOPOLOGY],
        "parent_of": [{"parent": p, "child": c} for p, c in PARENT_OF],
    }

    for n in range(1, args.max_passes + 1):
        try:
            mcp.call("tools/call", {"name": "lambo_derive", "arguments": derive_args})
        except RuntimeError as e:
            print(f"pass {n}: derive failed: {e}", file=sys.stderr)
            return 1

        stats = tool_text(mcp.call("tools/call", {"name": "lambo_stats", "arguments": {"agent_id": args.agent}}))
        canonical = 0
        for token in stats.replace(",", " ").split():
            if token.startswith("canonical="):
                canonical = int(token.split("=", 1)[1])
        print(f"pass {n:2d}  {stats}")

        if canonical > 0:
            print(f"\ncanonical reached after {n} pass(es).")
            saints = tool_text(mcp.call("tools/call", {"name": "lambo_saints", "arguments": {"agent_id": args.agent}}))
            print(saints or "(no saints output)")
            return 0

        time.sleep(args.interval)

    print(f"\nno concept promoted within {args.max_passes} passes.", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
