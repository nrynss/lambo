#!/usr/bin/env python3
"""Round-2 metrics — extends evidence/swarm/control/compute_metrics.py with the
acting/inert split the coordinator's round-1 review demanded.

This is a separate file under evidence/swarm/experiment2/ rather than an edit
to evidence/swarm/control/compute_metrics.py, because round 2's rules say not
to touch existing evidence outside evidence/swarm/experiment2/. The
computation is the same code path applied to every ledger passed on the
command line (round-1 treatment, round-1 control, round-2 arm A, round-2 arm
B), so all four columns in the README are computed identically rather than
some being quoted from prose.

New in this version, vs the round-1 script:
  - A task is "acting" when its `task` record's `n_tool_calls >= 1` (the
    model made at least one tool call before finishing the task) and "inert"
    when it made zero. The round-1 unconditional recall-first rate conflated
    protocol adherence with whether the model acted at all; this version
    reports both, with acting-conditioned recall-first as the PRIMARY metric
    (see `recall_first_pct_of_acting` below) per the coordinator's
    instruction.
  - `tool_call_err_by_class`: a coarse split of tool errors into "arg-error"
    (deserialize/validation failures — a client-side problem) vs "no-match"
    (`lambo_inspect` refusing because the focus doesn't exist) vs "other",
    useful for the arm-B lambo_inspect asymmetry noted in round 1.

Field semantics carried over unchanged from round 1 (re-verified against
these ledgers before computing anything):
  - `task` records' `recall_first` / `derives_without_prior_recall` are the
    driver's own per-task accounting; taken as given, not recomputed from
    `calls`.
  - "ok" call = `ok == true and is_error == false`.
  - Dedup rate = matched-existing / successful-derive-calls, parsed from the
    server's own response text via the same regex check_durability.py uses.
  - Unparseable/empty turn = a `model_turn` record with empty `content` and
    no `tool_calls`.

Usage: python3 compute_metrics2.py <ledger.jsonl> [<ledger.jsonl> ...]
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
    acting_tasks = [t for t in tasks if t.get("n_tool_calls", 0) >= 1]
    inert_tasks = [t for t in tasks if t.get("n_tool_calls", 0) < 1]
    n_acting = len(acting_tasks)
    n_inert = len(inert_tasks)

    recall_first_all = sum(1 for t in tasks if t.get("recall_first"))
    recall_first_acting = sum(1 for t in acting_tasks if t.get("recall_first"))

    derives_wo_prior_recall = sum(t.get("derives_without_prior_recall", 0) for t in tasks)

    tool_breakdown = Counter()
    tool_ok = Counter()
    tool_err = Counter()
    err_class = Counter()
    for c in calls:
        tool_breakdown[c["tool"]] += 1
        if c["ok"] and not c["is_error"]:
            tool_ok[c["tool"]] += 1
        else:
            tool_err[c["tool"]] += 1
            text = (c.get("text") or "")
            if "missing field" in text or "deserialize" in text:
                err_class["arg-error"] += 1
            elif "no concept matching" in text:
                err_class["no-match"] += 1
            elif "matches" in text and "concepts" in text and "name one exactly" in text:
                err_class["ambiguous-focus"] += 1
            else:
                err_class["other"] += 1

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

    http500 = sum(
        1 for e in model_errors if "HTTP Error 500" in (e.get("error") or "")
    )

    return {
        "ledger": path,
        "meta": meta,
        "tasks_total": n_tasks,
        "tasks_acting": n_acting,
        "tasks_inert": n_inert,
        "tasks_acting_pct": round(100 * n_acting / n_tasks, 1) if n_tasks else None,
        "recall_first_unconditional": recall_first_all,
        "recall_first_unconditional_pct": round(100 * recall_first_all / n_tasks, 1) if n_tasks else None,
        "recall_first_of_acting": recall_first_acting,
        "recall_first_pct_of_acting": round(100 * recall_first_acting / n_acting, 1) if n_acting else None,
        "derive_calls_total": n_derive_calls_total,
        "derives_without_prior_recall": derives_wo_prior_recall,
        "tool_call_breakdown": dict(tool_breakdown),
        "tool_call_ok": dict(tool_ok),
        "tool_call_err": dict(tool_err),
        "tool_call_err_by_class": dict(err_class),
        "calls_total": n_calls_total,
        "calls_ok": n_calls_ok,
        "calls_err": n_calls_err,
        "success_derive_calls": success_derive,
        "derive_created": derive_created,
        "derive_matched": derive_matched,
        "dedup_rate": round(dedup_rate, 3) if dedup_rate is not None else None,
        "model_turns_total": len(model_turns),
        "unparseable_or_empty_turns": unparseable_turns,
        "model_errors_total": len(model_errors),
        "model_errors_http500": http500,
    }


if __name__ == "__main__":
    for p in sys.argv[1:]:
        print(json.dumps(compute(p), indent=2))
