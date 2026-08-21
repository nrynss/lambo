#!/usr/bin/env python3
"""DOGFOOD metric 5 — every blast-radius warning fired, and what happened next.

The question: **did the warning change anything?** A blast-radius warning that
nobody acts on is decoration. Before I1 a fired warning existed only in the
conversation that received it, which by this repo's standards means it was
unclaimable — this script is the claimable version.

Reported for every recall that rendered a warning: when, to whom, over which
concept, at what blast radius, and whether the concept's block actually made it
into the context the model read.

**`included_in_context` cuts the block, not the warning.** The four hit-owned
warning kinds go into the flat `warnings` vector for every returned hit whatever
the token budget did (`src/recall/assemble.rs`: "a block truncated from the
context still reports its conditions"), and reach the agent as a second text
block. So a warning on a cut hit *was* delivered — the model read the warning line
and did not read the concept it was about. That is a weaker delivery, not a
missing one, and this report distinguishes them rather than calling the second one
unseen.

The `[canonical]` marker is the opposite case and is reported separately: it
renders **only** inside a hit's block, so a Canonical hit the budget cut carried no
marker at all. The ledger's set-level `canonical_marker` flag is gated on
`included_in_context` for that reason (`recall_facts` in `src/mcp/server.rs`), and
this report counts the cut-canonical hits so the gap is visible rather than merely
absent.

The honest part, and the reason this is metric 5's "honest version": with
`--repo <path>` each warning is joined against `git log` in the window after it.
**That join cannot prove causation and does not claim to.** What it can say is
whether commits landed in the window at all, and — as an explicitly-labelled
heuristic — whether any of their subjects or changed paths share a distinctive
token with the warned concept. A human reads the candidate rows; the script
never concludes "the agent ignored the warning".

Usage:
    python3 scripts/observability/blast_radius.py ~/lambo-dogfood/calls.jsonl
    python3 scripts/observability/blast_radius.py --repo . --window-minutes 120 calls.jsonl
"""

from __future__ import annotations

import datetime as dt
import re
import subprocess
import sys
from collections import Counter
from typing import Any

import _ledger

#: Hit-owned annotation kinds, from `AnnotationKind` in `src/recall/detail.rs`.
#: `load_bearing` IS the blast-radius warning (spec §13's load-bearing pillar).
HIT_KINDS = ("load_bearing", "conflict", "hot", "reservation")

#: Tokens too common to mean anything in a git-log match.
STOPWORDS = frozenset(
    """a an and are as at be by for from has have in into is it its of on or that the
    to was were will with must should not no do does this these those we our you your
    concept lambo""".split()
)


def distinctive_tokens(text: str) -> set[str]:
    """Words worth matching a commit against: alphanumeric, 4+ chars, not common."""
    return {
        w
        for w in re.findall(r"[a-z0-9_]{4,}", (text or "").lower())
        if w not in STOPWORDS
    }


def analyse(ledger: _ledger.Ledger) -> dict[str, Any]:
    events: list[dict[str, Any]] = []
    by_kind: Counter[str] = Counter()
    by_agent: Counter[str] = Counter()
    by_concept: Counter[str] = Counter()
    recalls = 0
    recalls_with_a_warning = 0
    #: Canonical hits the token budget cut, so `[canonical]` never rendered for
    #: them. The counterpart of the warning case above: for these, the marker was
    #: NOT delivered in any form.
    canonical_cut: list[dict[str, Any]] = []
    #: Recalls that returned a Canonical hit but reported `canonical_marker:
    #: false` — the flag and the hits disagreeing is exactly the budget case, and
    #: seeing it counted is how the flag's definition stays checkable from a file.
    recalls_with_a_canonical_hit = 0
    recalls_that_rendered_a_marker = 0

    for call in ledger.sorted_calls():
        if call.get("tool") != _ledger.RECALL_TOOL or not _ledger.succeeded(call):
            continue
        recalls += 1
        hits_all = call.get("hits") or []
        if any(h.get("is_canonical") for h in hits_all):
            recalls_with_a_canonical_hit += 1
        if call.get("canonical_marker") is True:
            recalls_that_rendered_a_marker += 1
        for h in hits_all:
            if h.get("is_canonical") and h.get("included_in_context") is False:
                canonical_cut.append(
                    {
                        "ts": call["ts"],
                        "agent_id": call.get("agent_id"),
                        "query": call.get("query"),
                        "node_id": h.get("node_id"),
                        "content": h.get("content"),
                        "blast_radius": h.get("blast_radius"),
                        "score": h.get("score"),
                    }
                )
        fired = False
        for hit in call.get("hits") or []:
            kinds = [k for k in (hit.get("annotations") or []) if k in HIT_KINDS]
            if not kinds:
                continue
            fired = True
            for kind in kinds:
                by_kind[kind] += 1
            by_agent[call.get("agent_id", "?")] += 1
            by_concept[hit.get("content") or hit.get("node_id", "?")] += 1
            events.append(
                {
                    "ts": call["ts"],
                    "agent_id": call.get("agent_id"),
                    "query": call.get("query"),
                    "node_id": hit.get("node_id"),
                    "content": hit.get("content"),
                    "kinds": kinds,
                    "blast_radius": hit.get("blast_radius"),
                    "is_canonical": hit.get("is_canonical"),
                    "included_in_context": hit.get("included_in_context"),
                    "score": hit.get("score"),
                }
            )
        if fired:
            recalls_with_a_warning += 1

        # Response-global annotations are never hit-owned (the H3 contract), so
        # they are counted here rather than per hit.
        for kind in call.get("response_annotations") or []:
            by_kind[f"response:{kind}"] += 1

    blast = [e for e in events if "load_bearing" in e["kinds"]]
    return {
        "recalls": recalls,
        "recalls_with_a_warning": recalls_with_a_warning,
        "warning_events": events,
        "blast_radius_warnings": blast,
        # The warning line reached the agent; the BLOCK it was about did not.
        "blast_radius_warnings_whose_block_was_cut": [
            e for e in blast if e["included_in_context"] is False
        ],
        "recalls_with_a_canonical_hit": recalls_with_a_canonical_hit,
        "recalls_that_rendered_a_canonical_marker": recalls_that_rendered_a_marker,
        "canonical_hits_cut_by_the_budget": canonical_cut,
        "by_kind": dict(by_kind),
        "by_agent": dict(by_agent),
        "top_concepts": by_concept.most_common(15),
    }


def git_join(
    events: list[dict[str, Any]], repo: str, window_minutes: float
) -> list[dict[str, Any]]:
    """For each warning, the commits in the window after it.

    Correlation by time window plus an explicitly-labelled token heuristic. It
    proves nothing about causation and the report says so.
    """
    out: list[dict[str, Any]] = []
    for event in events:
        when = _ledger.parse_ts(event["ts"])
        until = when + dt.timedelta(minutes=window_minutes)
        try:
            raw = subprocess.run(
                [
                    "git",
                    "-C",
                    repo,
                    "log",
                    f"--since={when.isoformat()}",
                    f"--until={until.isoformat()}",
                    "--name-only",
                    "--format=%x00%H%x1f%aI%x1f%s",
                ],
                capture_output=True,
                text=True,
                check=True,
                timeout=60,
            ).stdout
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
            out.append({**event, "git_error": str(exc), "commits": []})
            continue

        wanted = distinctive_tokens(event.get("content"))
        commits = []
        for chunk in raw.split("\x00"):
            chunk = chunk.strip("\n")
            if not chunk:
                continue
            head, _, files = chunk.partition("\n")
            parts = head.split("\x1f")
            if len(parts) != 3:
                continue
            sha, authored, subject = parts
            paths = [p for p in files.split("\n") if p.strip()]
            haystack = distinctive_tokens(subject) | {
                t for p in paths for t in distinctive_tokens(p)
            }
            overlap = sorted(wanted & haystack)
            commits.append(
                {
                    "sha": sha[:12],
                    "authored": authored,
                    "subject": subject,
                    "files": len(paths),
                    "token_overlap": overlap,
                }
            )
        out.append({**event, "commits": commits})
    return out


def render(data: dict[str, Any], joined: list[dict[str, Any]] | None, window: float) -> list[str]:
    blast = data["blast_radius_warnings"]
    out = [
        f"recalls: {data['recalls']}   "
        f"with at least one warning: {data['recalls_with_a_warning']}",
        f"blast-radius (load_bearing) warnings fired: {len(blast)}",
    ]
    cut = data["blast_radius_warnings_whose_block_was_cut"]
    if cut:
        out.append(
            f"   of which {len(cut)} were attached to a hit the TOKEN BUDGET CUT — "
            "the WARNING LINE still reached the agent (warnings are delivered "
            "independently of the budget); the concept BLOCK it was about did not"
        )

    # The marker is the other half of the same story, and the half that IS
    # budget-gated: `[canonical]` renders only inside a hit's block.
    out += [
        "",
        f"Canonical hits: {data['recalls_with_a_canonical_hit']} recall(s) returned one, "
        f"{data['recalls_that_rendered_a_canonical_marker']} rendered the [canonical] marker",
    ]
    marker_cut = data["canonical_hits_cut_by_the_budget"]
    if marker_cut:
        out.append(
            f"   {len(marker_cut)} Canonical hit(s) were CUT BY THE TOKEN BUDGET, so no "
            "[canonical] marker rendered for them at all — unlike a warning line, the "
            "marker lives inside the block and is not delivered separately. The ledger's "
            "`canonical_marker` flag is false for those recalls, which is why it is not a "
            "count of Canonical hits returned; read per-hit `is_canonical` for that."
        )
        for e in marker_cut[:10]:
            out.append(
                f"   {e['ts']}  {e['agent_id']}  blast_radius={e['blast_radius']}  "
                f"concept: {e['content']!r}"
            )
        if len(marker_cut) > 10:
            out.append(f"   … and {len(marker_cut) - 10} more")

    out += ["", "By kind:"]
    if not data["by_kind"]:
        out.append("   none")
    for kind, n in sorted(data["by_kind"].items(), key=lambda t: -t[1]):
        out.append(f"   {kind:<24} {n}")

    if data["by_agent"]:
        out += ["", "By agent (who was warned):"]
        for agent, n in sorted(data["by_agent"].items(), key=lambda t: -t[1]):
            out.append(f"   {agent[:30]:<30} {n}")

    if data["top_concepts"]:
        out += ["", "Most-warned-about concepts:"]
        for content, n in data["top_concepts"]:
            out.append(f"   {n:>4}x  {content}")

    if not blast:
        out += [
            "",
            "No blast-radius warning fired in this ledger. Either no Canonical "
            "concept was recalled, or none has dependents yet — metric 5 has "
            "nothing to report, which is a finding rather than a pass.",
        ]
        return out

    out += ["", "Every blast-radius warning, in order:"]
    for e in blast:
        # The warning was delivered either way; this says whether the concept
        # block it pointed at was in the context the model read.
        seen = (
            "block in context"
            if e["included_in_context"]
            else "warning delivered, BLOCK CUT BY TOKEN BUDGET"
        )
        out.append(
            f"   {e['ts']}  {e['agent_id']}  blast_radius={e['blast_radius']}  "
            f"score={e['score']:.4f}  [{seen}]"
        )
        out.append(f"       concept: {e['content']!r}")
        out.append(f"       query:   {e['query']!r}")

    if joined is None:
        out += [
            "",
            "No --repo given, so no git join. Pass --repo <path> to see which "
            "commits landed in the window after each warning.",
        ]
        return out

    out += [
        "",
        f"Git join — commits within {window:g} minutes AFTER each warning.",
        "  CORRELATION ONLY. A commit in the window is not evidence the agent",
        "  ignored the warning, and its absence is not evidence the agent heeded",
        "  it. `overlap` is a token heuristic between the concept text and the",
        "  commit subject/paths: a starting point for a human, never a verdict.",
    ]
    for e in joined:
        if "git_error" in e:
            out.append(f"   {e['ts']}  git failed: {e['git_error']}")
            continue
        out.append(f"   {e['ts']}  {e['content']!r}")
        if not e["commits"]:
            out.append("       no commits in the window")
        for c in e["commits"]:
            overlap = ", ".join(c["token_overlap"]) or "-"
            out.append(
                f"       {c['sha']}  {c['authored']}  ({c['files']} file(s))  "
                f"overlap[{overlap}]  {c['subject'][:70]}"
            )
    return out


def main(argv: list[str]) -> int:
    parser = _ledger.base_parser(__doc__ or "")
    parser.add_argument(
        "--repo",
        help="repository path to join blast-radius warnings against with `git log`",
    )
    parser.add_argument(
        "--window-minutes",
        type=float,
        default=120.0,
        help="how long after a warning to look for commits (default: %(default)s)",
    )
    args = parser.parse_args(argv)
    ledger = _ledger.load(args.ledger)
    data = analyse(ledger)
    joined = None
    if args.repo:
        joined = git_join(data["blast_radius_warnings"], args.repo, args.window_minutes)
        data["git_join"] = {
            "repo": args.repo,
            "window_minutes": args.window_minutes,
            "events": joined,
        }
    _ledger.emit(
        "metric 5 — blast-radius warnings fired",
        ledger,
        render(data, joined, args.window_minutes),
        data,
        args.json,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
