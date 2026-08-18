#!/usr/bin/env python3
"""C5 — real-model swarm, minimal LLM loop fallback.

The specified swarm (OMP agents driving lambo MCP tools with a local chat
model) is not feasible for the models probed so far: LFM2-350M cannot emit
tool calls (prose, `finish_reason=stop`, no `tool_calls`); Qwen3-0.6B emits
correct `tool_calls` at the raw protocol level but under OMP's harness calls
the wrong tool (`lsp`, hallucinated arguments — zero lambo interactions);
functiongemma-270m emits FunctionGemma-native `<start_function_call>`
markup that this llama.cpp build returns as prose, never as `tool_calls`
(probed and recorded in evidence/swarm/probes/). The spec's fallback
applies: a minimal LLM loop of llama.cpp `/v1/chat/completions` + the
streamable-HTTP MCP client pattern.

Each agent is a thread that loops:

  prompt the model (with the previous recall as context) ->
  model replies with one JSON object {"concepts": [...]} ->
  the loop calls lambo_derive (MCP) with those concepts ->
  lambo_recall (MCP) for the next turn's context ->
  every response goes to a JSONL ledger

The model supplies the content; the loop supplies the tool-calling, which
is the honest description of what these small models can do under a harness
with dozens of tool schemas in context. Tasks/hour and the canonization
dedup rate (created vs matched existing, from the server's own response
text) are measured from the ledger.

Usage:
  python3 scripts/loadtest/mcp_swarm.py \
      --session c-swarm-20260818 --agents 3 --duration 240 \
      --ledger evidence/swarm/ledger-<run>.jsonl \
      --token "$SWARM_TOKEN" \
      --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request

LLAMA_ENDPOINT = "http://127.0.0.1:8081/v1/chat/completions"
LLAMA_MODEL = "lfm2-350m"
PROTOCOL_VERSION = "2025-06-18"

SYSTEM = (
    "You are a memory agent for an infrastructure knowledge graph. Respond with "
    "EXACTLY one JSON object and nothing else, of the form "
    '{"concepts": [{"content": "<concept text>", "concept_type": '
    '"entity|logic|constraint|resource"}]}. List 2-4 concrete concepts about '
    "the subject you are asked about. Never wrap in markdown."
)

SWARM_TOPICS = [
    "the auth middleware guards the user schema",
    "the billing service retries failed charges",
    "the rate limit protects the public API",
    "the session store persists user state",
    "the migration script upgrades the schema",
    "the cache layer speeds up recall",
]


class Ledger:
    def __init__(self, path: str) -> None:
        self._lock = threading.Lock()
        self._fh = open(path, "a", encoding="utf-8")

    def write(self, record: dict) -> None:
        line = json.dumps(record, ensure_ascii=True, separators=(",", ":")) + "\n"
        with self._lock:
            self._fh.write(line)
            self._fh.flush()

    def close(self) -> None:
        with self._lock:
            self._fh.close()


class Mcp:
    """Streamable-HTTP MCP client, same pattern as mcp_load.py."""

    def __init__(self, endpoint: str, token: str | None) -> None:
        self.endpoint = endpoint
        self.session: str | None = None
        self.token = token
        self._id = 0

    def _headers(self) -> dict[str, str]:
        h = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session:
            h["Mcp-Session-Id"] = self.session
        if self.token:
            h["Authorization"] = f"Bearer {self.token}"
        return h

    def _post(self, payload: dict) -> dict | None:
        req = urllib.request.Request(
            self.endpoint, data=json.dumps(payload).encode(), headers=self._headers(), method="POST"
        )
        with urllib.request.urlopen(req, timeout=120) as r:
            sid = r.headers.get("Mcp-Session-Id")
            if sid:
                self.session = sid
            if "text/event-stream" not in (r.headers.get("Content-Type") or ""):
                body = r.read().decode().strip()
                return json.loads(body) if body else None
            for raw in r:
                line = raw.decode().strip()
                if line.startswith("data:") and line[5:].strip().startswith("{"):
                    return json.loads(line[5:].strip())
        raise RuntimeError("stream ended without a reply")

    def initialize(self) -> None:
        reply = self._post(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "c-series-swarm", "version": "1"},
                },
            }
        )
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call_tool(self, name: str, arguments: dict) -> tuple[bool, str]:
        self._id += 1
        reply = self._post(
            {
                "jsonrpc": "2.0",
                "id": self._id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        if reply is None:
            return False, "empty reply"
        if "error" in reply:
            return True, json.dumps(reply["error"], ensure_ascii=True)
        result = reply.get("result", {})
        text = " ".join(
            c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"
        )
        return not bool(result.get("isError")), text.strip()


def model_reply(prompt: str, token: str, endpoint: str, model: str) -> str:
    """One /v1/chat/completions turn. Returns the raw content."""
    body = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": prompt},
            ],
            "max_tokens": 400,
            "temperature": 0.7,
        }
    ).encode()
    req = urllib.request.Request(
        endpoint, data=body, headers={"Content-Type": "application/json",
                                      "Authorization": f"Bearer {token}"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read().decode())
    return (data["choices"][0]["message"].get("content") or "").strip()


def extract_concepts(reply: str) -> list[dict]:
    """Pull the first JSON object out of the model's reply."""
    start, end = reply.find("{"), reply.rfind("}")
    if start == -1 or end <= start:
        return []
    try:
        obj = json.loads(reply[start : end + 1])
    except json.JSONDecodeError:
        return []
    out = []
    for c in obj.get("concepts", [])[:4]:
        content = str(c.get("content", "")).strip()
        ctype = str(c.get("concept_type", "entity")).strip()
        if content and ctype in ("entity", "logic", "constraint", "resource"):
            out.append({"content": content, "concept_type": ctype})
    return out


def agent_loop(idx: int, ledger: Ledger, args: argparse.Namespace, stop: threading.Event) -> None:
    agent_id = f"swarm-{idx}"
    mcp = Mcp(args.endpoint, args.token)
    mcp.initialize()
    topic = SWARM_TOPICS[idx % len(SWARM_TOPICS)]
    context = ""
    seq = 0
    while not stop.is_set():
        seq += 1
        prompt = f"Derive concepts about: {topic}.\nContext from recall: {context or '(none yet)'}"
        try:
            reply = model_reply(prompt, args.llama_key, args.llama_endpoint, args.llama_model)
        except Exception as e:
            ledger.write(
                {"kind": "model_error", "worker": idx, "agent": agent_id, "seq": seq,
                 "error": f"{type(e).__name__}: {e}", "t": time.time()}
            )
            time.sleep(1.0)
            continue
        concepts = extract_concepts(reply)
        if not concepts:
            ledger.write(
                {"kind": "model_reply", "worker": idx, "agent": agent_id, "seq": seq,
                 "reply": reply[:500], "parsed_concepts": 0, "t": time.time()}
            )
            time.sleep(0.5)
            continue
        ok, text = mcp.call_tool(
            "lambo_derive",
            {"agent_id": agent_id, "concepts": concepts},
        )
        ledger.write(
            {"kind": "derive", "worker": idx, "agent": agent_id, "seq": seq,
             "concepts": concepts, "ok": ok, "text": text[:500], "t": time.time()}
        )
        if not ok:
            time.sleep(0.5)
            continue
        ok, recall = mcp.call_tool(
            "lambo_recall", {"agent_id": agent_id, "query": topic}
        )
        context = recall[:600]
        ledger.write(
            {"kind": "recall", "worker": idx, "agent": agent_id, "seq": seq,
             "ok": ok, "text": recall[:500], "t": time.time()}
        )
        time.sleep(args.turn_gap)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--session", required=True)
    ap.add_argument("--ledger", required=True)
    ap.add_argument("--endpoint", default="http://127.0.0.1:7701/mcp")
    ap.add_argument("--token", default=None)
    ap.add_argument("--llama-key", default="lambo-swarm-local")
    ap.add_argument("--agents", type=int, default=3)
    ap.add_argument("--duration", type=float, default=240.0)
    ap.add_argument("--turn-gap", type=float, default=2.0)
    ap.add_argument("--llama-endpoint", default=LLAMA_ENDPOINT,
                    help="llama.cpp /v1/chat/completions base URL")
    ap.add_argument("--llama-model", default=LLAMA_MODEL,
                    help="model id to request from the llama server")
    args = ap.parse_args()
    args.token = args.token or os.environ.get("LAMBO_AUTH_TOKEN")
    args.llama_endpoint = args.llama_endpoint.rstrip("/") + "/chat/completions"

    ledger = Ledger(args.ledger)
    ledger.write(
        {"kind": "meta", "session": args.session, "agents": args.agents,
         "model": args.llama_model, "llama_endpoint": args.llama_endpoint,
         "started_at": time.time(), "duration": args.duration}
    )
    stop = threading.Event()
    threads = [
        threading.Thread(target=agent_loop, args=(i, ledger, args, stop), daemon=True)
        for i in range(args.agents)
    ]
    for t in threads:
        t.start()
    time.sleep(args.duration)
    stop.set()
    time.sleep(1.0)
    ledger.write({"kind": "done", "t": time.time()})
    ledger.close()
    print(f"swarm done: {args.agents} agents x {args.duration}s — ledger {args.ledger}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
