#!/usr/bin/env python3
"""Control-arm metrics — computed identically for the treatment and control ledgers.

Reads a `kind":"call"` / `kind":"task"` / `kind":"model_turn"` /
`kind":"model_error"` JSONL ledger produced by scripts/loadtest/mcp_agentic.py
and prints the same set of numbers for either arm, so the two columns in
evidence/swarm/control/README.md are computed by one code path rather than
one side being quoted from prose and the other freshly computed.

Field semantics (confirmed against the treatment ledger before writing this):
  - kind":"task" records carry the per-task accounting the driver itself
    computed: `recall_first` (bool: task's first executed call was
    lambo_recall) and `derives_without_prior_recall` (int, computed by the
    driver as calls that are lambo_derive with no lambo_recall earlier in
    the same task's call list). We take these as given rather than
    recomputing from `calls`, since that's what "same definitions" means.
  - kind":"call" records carry tool/ok/is_error/text per executed MCP call.
    A call is "ok" when ok==true and is_error==false.
  - Dedup rate = matched-existing / successful-derive-calls, parsed from the
    server's own response text via the same regex check_durability.py uses
    (`derived N concept(s): C created, M matched existing`).
  - Unparseable/empty turns = model_turn records with empty content AND no
    tool_calls.
  - model_error records = llama-server HTTP/transport failures.

Usage: python3 compute_metrics.py <ledger.jsonl>
"""
from __future__ import annotations

import json
import re
import sys
from collections import Counter

DERIVED_RE = re.compile(r"derived \d+ concept\(s\): (\d+) created, (\d+) matched existing")


def compute(path: str) -> dict:
    tasks = []
    calls = []
    model_turns = []
    model_errors = []
    meta = None
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        r = json.loads(line)
        k = r.get("kind")
        if k == "meta":
            meta = r
        elif k == "task":
            tasks.append(r)
        elif k == "call":
            calls.append(r)
        elif k == "model_turn":
            model_turns.append(r)
        elif k == "model_error":
            model_errors.append(r)

    n_tasks = len(tasks)
    recall_first = sum(1 for t in tasks if t.get("recall_first"))
    derives_wo_prior_recall = sum(t.get("derives_without_prior_recall", 0) for t in tasks)

    tool_breakdown = Counter()
    tool_ok = Counter()
    tool_err = Counter()
    for c in calls:
        tool_breakdown[c["tool"]] += 1
        if c["ok"] and not c["is_error"]:
            tool_ok[c["tool"]] += 1
        else:
            tool_err[c["tool"]] += 1

    n_derive_calls_total = tool_breakdown.get("lambo_derive", 0)

    success_derive = 0
    derive_created = 0
    derive_matched = 0
    for c in calls:
        if c["tool"] == "lambo_derive" and c["ok"] and not c["is_error"]:
            m = DERIVED_RE.search(c.get("text") or "")
            if m:
                success_derive += 1
                derive_created += int(m.group(1))
                derive_matched += int(m.group(2))
    dedup_rate = (derive_matched / success_derive) if success_derive else None

    unparseable_turns = sum(
        1 for m in model_turns
        if not (m.get("content") or "").strip() and not m.get("tool_calls")
    )

    n_calls_total = len(calls)
    n_calls_ok = sum(1 for c in calls if c["ok"] and not c["is_error"])
    n_calls_err = n_calls_total - n_calls_ok

    return {
        "ledger": path,
        "meta": meta,
        "tasks_total": n_tasks,
        "tasks_recall_first": recall_first,
        "tasks_recall_first_pct": round(100 * recall_first / n_tasks, 1) if n_tasks else None,
        "derive_calls_total": n_derive_calls_total,
        "derives_without_prior_recall": derives_wo_prior_recall,
        "tool_call_breakdown": dict(tool_breakdown),
        "tool_call_ok": dict(tool_ok),
        "tool_call_err": dict(tool_err),
        "calls_total": n_calls_total,
        "calls_ok": n_calls_ok,
        "calls_err": n_calls_err,
        "success_derive_calls": success_derive,
        "derive_created": derive_created,
        "derive_matched": derive_matched,
        "dedup_rate": round(dedup_rate, 3) if dedup_rate is not None else None,
        "model_turns_total": len(model_turns),
        "unparseable_or_empty_turns": unparseable_turns,
        "model_errors": len(model_errors),
    }


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(json.dumps(compute(p), indent=2))
