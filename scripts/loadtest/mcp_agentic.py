#!/usr/bin/env python3
"""C5M-R1-2 — genuine agentic re-run: the MODEL chooses the lambo tool calls.

The fallback swarm (`mcp_swarm.py`) hardcodes prompt -> model_reply ->
lambo_derive -> lambo_recall; the model never selects a tool. This script is
the agentic counterpart: the model is given the lambo-cloudops skill as its
system prompt (pre-flight recall protocol, provenance/derivation protocol,
blast-radius semantics, fail-closed rules) and a minimal toolset — the four
lambo MCP tools `lambo_derive`, `lambo_recall`, `lambo_record_action`,
`lambo_inspect` (schemas fetched live from the server's `tools/list`) — and
chooses every call itself via llama.cpp's OpenAI tools API (`/v1/chat/
completions` `tool_calls`). The harness only executes the calls the model
emits and feeds the server's responses back. If the model fails to follow the
protocol, that failure is recorded honestly (the derive-without-prior-recall
rate below is a measured finding, not a pass criterion).

Loop per agent thread:

  user task ("run the pre-flight protocol for resource X, then ...") ->
  model emits tool_calls (or a final text answer) ->
  the harness executes each call over MCP, appends the server's response as
  a `tool` message, and asks the model again ->
  a content-only reply ends the task; the next task starts.

Ledger records (JSONL, one per line):

  {"kind":"meta", ...}                     run header (skill file + sha256)
  {"kind":"model_turn", ...}               every model response: content and/or
                                           tool_calls emitted (with names+args)
  {"kind":"call", ...}                     every executed tool call — the fields
                                           check_durability.py accounts on
                                           (tool/ok/is_error/text/http_status)
  {"kind":"task", ...}                     task boundaries: n_tool_calls,
                                           recall_first (pre-flight recall was
                                           the task's first call),
                                           derives_without_prior_recall
  {"kind":"model_error", ...}              HTTP/transport failures on the model
                                           call
  {"kind":"done", ...}                     window expired

Durability: run the window, SIGTERM the lambo serve, then
`python3 scripts/loadtest/check_durability.py --ledger <ledger> --db <store>
--session <session> --stderr <stderr>` — the `call` records carry the
server's own "derived N concept(s): C created, M matched existing" response
texts, which is exactly what check_durability parses.

Usage:
  python3 scripts/loadtest/mcp_agentic.py \
      --session c-qwen3-agentic-20260818 \
      --ledger evidence/swarm/ledger-agentic-qwen3-<run>.jsonl \
      --endpoint http://127.0.0.1:7705/mcp --token "$LAMBO_AUTH_TOKEN" \
      --agents 3 --duration 150 \
      --skill skills/lambo-cloudops/SKILL.md \
      --llama-model qwen3-0.6b --llama-endpoint http://127.0.0.1:8082/v1 \
      --llama-key lambo-swarm-local
"""

from __future__ import annotations

import argparse
import hashlib
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
LAMBO_TOOLS = ["lambo_derive", "lambo_recall", "lambo_record_action", "lambo_inspect"]
MAX_TOOL_ROUNDS_PER_TASK = 12  # loop guard; the model may finish sooner

AGENTIC_TASKS = [
    "modify the 'auth middleware' resource: run the pre-flight recall protocol "
    "for it, derive the concept, record the action with its depends-on edges, "
    "then re-check with recall. Halt if recall shows a load-bearing pillar.",
    "modify the 'rate limit' resource: run the pre-flight recall protocol for it, "
    "derive the concept, record the action with its depends-on edges, then "
    "re-check with recall. Halt if recall shows a load-bearing pillar.",
    "modify the 'billing service' resource: run the pre-flight recall protocol "
    "for it, derive the concept, record the action with its depends-on edges, "
    "then re-check with recall. Halt if recall shows a load-bearing pillar.",
    "modify the 'session store' resource: run the pre-flight recall protocol for "
    "it, derive the concept, record the action with its depends-on edges, then "
    "re-check with recall. Halt if recall shows a load-bearing pillar.",
    "modify the 'migration script' resource: run the pre-flight recall protocol "
    "for it, derive the concept, record the action with its depends-on edges, "
    "then re-check with recall. Halt if recall shows a load-bearing pillar.",
    "modify the 'cache layer' resource: run the pre-flight recall protocol for "
    "it, derive the concept, record the action with its depends-on edges, then "
    "re-check with recall. Halt if recall shows a load-bearing pillar.",
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
    """Streamable-HTTP MCP client, same pattern as mcp_swarm.py / mcp_load.py."""

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
                    "clientInfo": {"name": "c5-agentic", "version": "1"},
                },
            }
        )
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def list_tools(self) -> list[dict]:
        reply = self._post({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        return reply.get("result", {}).get("tools", [])

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


def openai_tools(schemas: list[dict]) -> list[dict]:
    """Filter the server's tools/list to the minimal lambo toolset, OpenAI-style."""
    out = []
    by_name = {t["name"]: t for t in schemas}
    for name in LAMBO_TOOLS:
        t = by_name.get(name)
        if t is None:
            raise RuntimeError(f"server tools/list missing {name}")
        out.append(
            {
                "type": "function",
                "function": {
                    "name": name,
                    "description": t.get("description", ""),
                    "parameters": t.get("inputSchema", {"type": "object", "properties": {}}),
                },
            }
        )
    return out


def model_turn(
    messages: list[dict], tools: list[dict], token: str, endpoint: str, model: str
) -> dict:
    """One /v1/chat/completions turn. Returns the choices[0] message dict."""
    body = json.dumps(
        {
            "model": model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "max_tokens": 512,
            "temperature": 0.7,
        }
    ).encode()
    req = urllib.request.Request(
        endpoint,
        data=body,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read().decode())
    return data["choices"][0]["message"]


def agent_loop(idx: int, ledger: Ledger, args: argparse.Namespace, stop: threading.Event) -> None:
    agent_id = f"agentic-{idx}"
    mcp = Mcp(args.endpoint, args.token)
    mcp.initialize()
    schemas = mcp.list_tools()
    tools = openai_tools(schemas)
    task_idx = idx
    seq = 0
    while not stop.is_set():
        topic = args.task_list[task_idx % len(args.task_list)]
        task_idx += 1
        seq += 1
        messages: list[dict] = [
            {"role": "system", "content": args.skill_text},
            {"role": "user", "content": f"Task: {topic}"},
        ]
        calls = []  # executed tool calls this task, in order
        task_started = time.time()
        rounds = 0
        stop_reason = "model-done"
        while not stop.is_set():
            rounds += 1
            try:
                msg = model_turn(messages, tools, args.llama_key, args.llama_endpoint, args.llama_model)
            except Exception as e:
                ledger.write(
                    {"kind": "model_error", "worker": idx, "agent": agent_id, "seq": seq,
                     "error": f"{type(e).__name__}: {e}", "t": time.time()}
                )
                stop_reason = "model-error"
                time.sleep(1.0)
                break
            content = (msg.get("content") or "").strip()
            tcalls = msg.get("tool_calls") or []
            ledger.write(
                {"kind": "model_turn", "worker": idx, "agent": agent_id, "seq": seq,
                 "round": rounds, "content": content[:500],
                 "tool_calls": [
                     {"name": c["function"]["name"], "arguments": c["function"].get("arguments")}
                     for c in tcalls
                 ],
                 "t": time.time()}
            )
            if not tcalls:
                # The model chose to stop calling tools: task complete.
                break
            if rounds >= MAX_TOOL_ROUNDS_PER_TASK:
                stop_reason = "loop-guard"
                break
            for c in tcalls:
                name = c["function"]["name"]
                try:
                    arguments = json.loads(c["function"].get("arguments") or "{}")
                except json.JSONDecodeError:
                    arguments = {}
                ok, text = mcp.call_tool(name, arguments)
                calls.append(name)
                ledger.write(
                    {"kind": "call", "worker": idx, "agent": agent_id, "seq": seq,
                     "tool": name, "arguments": arguments, "ok": ok,
                     "is_error": not ok, "text": text[:500], "http_status": None,
                     "t": time.time()}
                )
                messages.append(
                    {"role": "assistant", "content": content or None, "tool_calls": [c]}
                )
                messages.append(
                    {"role": "tool", "tool_call_id": c.get("id", ""),
                     "content": text[:4000] or "(no text)"}
                )
        # Task accounting: did the pre-flight recall come first?
        recall_first = bool(calls) and calls[0] == "lambo_recall"
        derives = [i for i, n in enumerate(calls) if n == "lambo_derive"]
        derives_without_prior_recall = sum(
            1 for i in derives if not any(calls[j] == "lambo_recall" for j in range(i))
        )
        ledger.write(
            {"kind": "task", "worker": idx, "agent": agent_id, "seq": seq,
             "topic": topic, "reason": stop_reason, "n_tool_calls": len(calls),
             "calls": calls, "recall_first": recall_first,
             "derives_without_prior_recall": derives_without_prior_recall,
             "elapsed_s": round(time.time() - task_started, 3), "t": time.time()}
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
    ap.add_argument("--duration", type=float, default=150.0)
    ap.add_argument("--turn-gap", type=float, default=1.0)
    ap.add_argument("--skill", default="skills/lambo-cloudops/SKILL.md")
    ap.add_argument("--tasks", default=None,
                    help="path to a task-list file, one task per line; default (omitted) "
                    "is the built-in AGENTIC_TASKS, unchanged from before this flag existed")
    ap.add_argument("--llama-endpoint", default=LLAMA_ENDPOINT,
                    help="llama.cpp /v1/chat/completions base URL")
    ap.add_argument("--llama-model", default=LLAMA_MODEL,
                    help="model id to request from the llama server")
    args = ap.parse_args()
    args.token = args.token or os.environ.get("LAMBO_AUTH_TOKEN")
    args.llama_endpoint = args.llama_endpoint.rstrip("/") + "/chat/completions"
    with open(args.skill, encoding="utf-8") as fh:
        args.skill_text = fh.read()
    skill_sha = hashlib.sha256(args.skill_text.encode()).hexdigest()
    if args.tasks:
        with open(args.tasks, encoding="utf-8") as fh:
            args.task_list = [line.strip() for line in fh if line.strip()]
        if not args.task_list:
            raise SystemExit(f"--tasks {args.tasks}: no non-empty lines")
    else:
        args.task_list = AGENTIC_TASKS

    ledger = Ledger(args.ledger)
    meta_record = {"kind": "meta", "session": args.session, "agents": args.agents,
         "model": args.llama_model, "llama_endpoint": args.llama_endpoint,
         "skill": args.skill, "skill_sha256": skill_sha,
         "tools": LAMBO_TOOLS, "started_at": time.time(), "duration": args.duration}
    if args.tasks:
        meta_record["tasks"] = args.tasks
        meta_record["tasks_sha256"] = hashlib.sha256(
            "\n".join(args.task_list).encode()
        ).hexdigest()
    ledger.write(meta_record)
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
    print(f"agentic run done: {args.agents} agents x {args.duration}s — ledger {args.ledger}",
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
