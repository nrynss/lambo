#!/usr/bin/env python3
"""DOGFOOD metric 1 — recall-first compliance, per agent, per work session.

The question: **did the agent load memory before it wrote memory?** An agent
that derives without recalling first is not using the graph, it is filling it.

Definitions, stated here because a metric computed two ways is a metric nobody
can quote:

  * A **write** is a successful `lambo_derive` or `lambo_record_action`.
  * A **write sequence** is a maximal run of consecutive writes by one agent
    inside one work session (a read between two writes ends the run). Counting
    sequences rather than individual writes is deliberate: an agent that recalls
    once and then derives six concepts complied once, and scoring it six times
    would flatter it exactly as much as scoring it once would punish it.
  * A write sequence is **recall-first** when a successful `lambo_recall`
    appears earlier in the same work session.
  * A **work session** is a stretch of one agent's calls with no gap longer than
    --gap-minutes and no serve restart inside it. The ledger records no
    agent-session boundary — `serve` holds one lambo session for its whole life
    — so the gap is an explicit proxy; the restart is a fact, read off the
    heartbeat's uptime resetting.

Also reported, because it is the number C5 measured and the one directly
comparable to `evidence/swarm/`: **derives with no prior recall in their work
session**, counted per call rather than per sequence.

Usage:
    python3 scripts/observability/recall_first.py ~/lambo-dogfood/calls.jsonl
    python3 scripts/observability/recall_first.py --gap-minutes 60 --json calls.jsonl
"""

from __future__ import annotations

import sys
from typing import Any

import _ledger
from _ledger import RECALL_TOOL, WRITE_TOOLS


def analyse(ledger: _ledger.Ledger, gap_minutes: float) -> dict[str, Any]:
    restarts = ledger.restart_times()
    calls = ledger.sorted_calls()

    per_agent: dict[str, dict[str, Any]] = {}
    for agent in ledger.agents():
        mine = [c for c in calls if c.get("agent_id") == agent]
        sessions = list(_ledger.work_sessions(mine, gap_minutes, restarts))
        rows = []
        for session in sessions:
            recalled = False
            sequences = 0
            compliant = 0
            derives_without_recall = 0
            in_sequence = False
            for call in session:
                tool = call.get("tool")
                ok = _ledger.succeeded(call)
                if tool == RECALL_TOOL and ok:
                    recalled = True
                    in_sequence = False
                    continue
                if tool in WRITE_TOOLS and ok:
                    if not in_sequence:
                        sequences += 1
                        in_sequence = True
                        if recalled:
                            compliant += 1
                    if tool == "lambo_derive" and not recalled:
                        derives_without_recall += 1
                    continue
                # Any other call (a read, or a failed write) ends the run
                # without starting one.
                in_sequence = False
            rows.append(
                {
                    "started": session[0]["ts"],
                    "ended": session[-1]["ts"],
                    "calls": len(session),
                    "write_sequences": sequences,
                    "recall_first_sequences": compliant,
                    "derives_without_prior_recall": derives_without_recall,
                    "opened_with_recall": session[0].get("tool") == RECALL_TOOL,
                }
            )
        seqs = sum(r["write_sequences"] for r in rows)
        good = sum(r["recall_first_sequences"] for r in rows)
        per_agent[agent] = {
            "calls": len(mine),
            "work_sessions": len(rows),
            "write_sequences": seqs,
            "recall_first_sequences": good,
            "compliance": (good / seqs) if seqs else None,
            "derives_without_prior_recall": sum(
                r["derives_without_prior_recall"] for r in rows
            ),
            "sessions_opened_with_recall": sum(1 for r in rows if r["opened_with_recall"]),
            "sessions": rows,
        }

    total_seq = sum(a["write_sequences"] for a in per_agent.values())
    total_good = sum(a["recall_first_sequences"] for a in per_agent.values())
    return {
        "gap_minutes": gap_minutes,
        "restarts": [r.isoformat() for r in restarts],
        "agents": per_agent,
        "total_write_sequences": total_seq,
        "total_recall_first_sequences": total_good,
        "overall_compliance": (total_good / total_seq) if total_seq else None,
        "total_derives_without_prior_recall": sum(
            a["derives_without_prior_recall"] for a in per_agent.values()
        ),
    }


def render(data: dict[str, Any]) -> list[str]:
    out = [
        f"work-session gap: {data['gap_minutes']:g} min "
        f"({len(data['restarts'])} serve restart(s) also split sessions)",
        "",
        f"{'agent':<24} {'calls':>6} {'sess':>5} {'wseq':>5} {'r-first':>8} "
        f"{'compliance':>11} {'derive-no-recall':>17}",
        "-" * 82,
    ]
    for agent, a in sorted(data["agents"].items()):
        compliance = "n/a" if a["compliance"] is None else f"{a['compliance'] * 100:.1f}%"
        out.append(
            f"{agent[:24]:<24} {a['calls']:>6} {a['work_sessions']:>5} "
            f"{a['write_sequences']:>5} {a['recall_first_sequences']:>8} "
            f"{compliance:>11} {a['derives_without_prior_recall']:>17}"
        )
    out.append("-" * 82)
    overall = data["overall_compliance"]
    out.append(
        f"{'ALL':<24} {'':>6} {'':>5} {data['total_write_sequences']:>5} "
        f"{data['total_recall_first_sequences']:>8} "
        f"{('n/a' if overall is None else f'{overall * 100:.1f}%'):>11} "
        f"{data['total_derives_without_prior_recall']:>17}"
    )

    if data["total_write_sequences"] == 0:
        out += ["", "No write sequences in this ledger — nothing to comply with."]
        return out

    out += ["", "Per work session (the sessions that broke compliance first):"]
    offenders = [
        (agent, s)
        for agent, a in data["agents"].items()
        for s in a["sessions"]
        if s["write_sequences"] > s["recall_first_sequences"]
    ]
    if not offenders:
        out.append("   none — every write sequence was preceded by a recall")
    for agent, s in sorted(offenders, key=lambda t: t[1]["started"])[:20]:
        out.append(
            f"   {s['started']}  {agent:<20} "
            f"{s['recall_first_sequences']}/{s['write_sequences']} sequences recall-first, "
            f"{s['derives_without_prior_recall']} derive(s) with no prior recall"
        )
    if len(offenders) > 20:
        out.append(f"   … and {len(offenders) - 20} more")
    return out


def main(argv: list[str]) -> int:
    parser = _ledger.base_parser(__doc__ or "")
    parser.add_argument(
        "--gap-minutes",
        type=float,
        default=_ledger.DEFAULT_SESSION_GAP_MINUTES,
        help="idle gap that starts a new work session (default: %(default)s)",
    )
    args = parser.parse_args(argv)
    ledger = _ledger.load(args.ledger)
    data = analyse(ledger, args.gap_minutes)
    _ledger.emit(
        "metric 1 — recall-first compliance", ledger, render(data), data, args.json
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
