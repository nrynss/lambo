#!/usr/bin/env python3
"""DOGFOOD metric 2 — re-derivation savings: derive created vs matched over time.

The question: **is the graph converging or accumulating?** Every `matched` is a
concept the agent would otherwise have written again — the saving memory exists
to produce. Every `created` is either genuinely new information or a duplicate
that got past canonicalization (which is metric 3's job, `duplicates.py`).

    dedup rate = matched / (created + matched)

The trend is the point, not the number: a session whose dedup rate climbs is
converging on a stable vocabulary. `--bucket` sets the time axis.

An **absent** fact is not a zero. A successful derive line carrying no
created/matched keys at all is counted and reported on its own line rather than
folded in as `created=0, matched=0` — the difference between "nothing was
re-derived" and "this reader no longer understands these lines".

**MCP-driven sessions are read through the completion join (J4).** J3 made
`lambo_derive` return before the write is applied, so its call line cannot carry
counts that do not exist yet; J4 put them back on a `kind:"completion"` line
carrying the same `receipt`. This report joins on that key, so an MCP session's
dedup rate is a real number again rather than `n/a`. `_ledger.joined_facts` is
the one implementation of the join, shared with `duplicates.py`, and it reports
which of the two sources each call's facts came from — the summary prints the
split, because "the ledger has no completions" and "these writes were
synchronous" are different states that used to look identical.

`semantic_merged` is reported separately and NOT counted as a match. A hybrid
similarity merge adds a decaying `Semantic` edge and does **not** re-upsert the
target or add a `Derives` edge, so folding it into `matched` would overstate
re-derivation savings with a weaker relationship. See `DeriveOutcome` in
`src/graph/derive.rs`. **It is unavailable for joined calls**, and reported as
unavailable rather than as zero: the completion line carries the created/matched
pair and no more, so a joined session's `sem.merged` column is a genuine
absence.

With `--store <path>` the ledger's own arithmetic is checked against the SQLite
store: created-minus-matched should land near the store's concept count. A large
shortfall is normally the daemon's GC having swept unreferenced concepts (the
C1–C3 capture saw exactly this), not a broken ledger — but it should be *seen*
rather than assumed either way.

Usage:
    python3 scripts/observability/dedup_rate.py ~/lambo-dogfood/calls.jsonl
    python3 scripts/observability/dedup_rate.py --bucket hour --store ~/lambo.db calls.jsonl
"""

from __future__ import annotations

import sqlite3
import sys
from collections import defaultdict
from typing import Any

import _ledger

#: Prefix length of the RFC3339 stamp that identifies each bucket.
#:
#: This is a **dependency on the shape of `ts`**, invisible at the slicing site:
#: `2026-08-18T09` is 13 characters and `2026-08-18` is 10 only for the
#: fixed-width UTC form chrono's `to_rfc3339()` emits. A producer that switched to
#: local offsets would bucket by local wall clock while `sorted_calls` still
#: ordered by the string; one that changed field widths would slice mid-field. See
#: `_ledger.py`'s module docstring, where both timestamp-shape dependencies are
#: recorded together.
BUCKETS = {"hour": 13, "day": 10, "all": 0}

#: The derive facts this report reads. `matched` and `created` are the dedup rate;
#: the other two are reported beside it and deliberately not folded in.
DERIVE_FACTS = ("created", "matched", "semantic_merged", "reinforced")


def analyse(ledger: _ledger.Ledger, bucket: str, store: str | None) -> dict[str, Any]:
    width = BUCKETS[bucket]
    per_bucket: dict[str, dict[str, int]] = defaultdict(
        lambda: {"calls": 0, "created": 0, "matched": 0, "semantic_merged": 0, "reinforced": 0}
    )
    per_agent: dict[str, dict[str, int]] = defaultdict(
        lambda: {"calls": 0, "created": 0, "matched": 0, "semantic_merged": 0}
    )
    failed = 0
    # A successful derive line that carries NO created/matched facts at all — on
    # the line OR on a joined completion — is not the same thing as one that
    # created and matched nothing, and folding the two together is how a schema
    # change becomes a wrong number rather than a message: a `v:2` that renamed
    # `matched` would leave every line fact-less and every rate reading 0.000.
    # Counted separately and reported.
    factless = 0
    # Provenance of the facts that WERE found, because "no completions in this
    # file" and "these writes were synchronous" used to look identical (J4).
    from_line = 0
    from_completion = 0
    completions = ledger.completions_by_receipt()
    # `record_action` fans its produces/modifies/depends_on out into concepts too,
    # so the store cross-check below must count them or it will always report a
    # phantom surplus. They are NOT part of the dedup rate: `record_action` has
    # no matched/created split to compute one from.
    action_created = 0
    for call in ledger.sorted_calls():
        if call.get("tool") == "lambo_record_action" and _ledger.succeeded(call):
            # Same join: an MCP-driven record_action's `created` moved to the
            # completion at J3/J4, and without this the store cross-check below
            # reports a phantom surplus for every async session.
            facts, _ = _ledger.joined_facts(call, completions)
            action_created += facts.get("created", 0)
            continue
        if call.get("tool") != "lambo_derive":
            continue
        if not _ledger.succeeded(call):
            failed += 1
            continue
        key = call["ts"][:width] if width else "all"
        row = per_bucket[key]
        row["calls"] += 1
        agent_row = per_agent[call.get("agent_id", "?")]
        agent_row["calls"] += 1
        facts, source = _ledger.joined_facts(call, completions)
        for name in DERIVE_FACTS:
            value = facts.get(name)
            if isinstance(value, int):
                row[name] += value
                if name in agent_row:
                    agent_row[name] += value
        if source == "none":
            factless += 1
        elif source == "completion":
            from_completion += 1
        else:
            from_line += 1

    def rate(row: dict[str, int]) -> float | None:
        total = row["created"] + row["matched"]
        return (row["matched"] / total) if total else None

    buckets = [
        {"bucket": k, **v, "dedup_rate": rate(v)} for k, v in sorted(per_bucket.items())
    ]
    totals = {"calls": 0, "created": 0, "matched": 0, "semantic_merged": 0, "reinforced": 0}
    for row in per_bucket.values():
        for k in totals:
            totals[k] += row[k]

    data: dict[str, Any] = {
        "bucket": bucket,
        "buckets": buckets,
        "agents": {
            a: {**v, "dedup_rate": rate({**v, "matched": v["matched"]})}
            for a, v in sorted(per_agent.items())
        },
        "totals": {**totals, "dedup_rate": rate(totals)},
        "failed_derive_calls": failed,
        "derive_calls_without_facts": factless,
        "derive_facts_from_line": from_line,
        "derive_facts_from_completion": from_completion,
        "record_action_created": action_created,
    }

    if store:
        data["store"] = store_check(store, totals["created"] + action_created)
    return data


def store_check(path: str, created: int) -> dict[str, Any]:
    """Compare every concept the ledger saw created against the store's count."""
    try:
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    except sqlite3.Error as exc:
        return {"error": f"could not open {path}: {exc}"}
    try:
        concepts = con.execute("SELECT count(*) FROM concepts").fetchone()[0]
        with_embedding = con.execute(
            "SELECT count(*) FROM concepts WHERE embedding IS NOT NULL"
        ).fetchone()[0]
    except sqlite3.Error as exc:
        return {"error": f"{path} does not look like a provisioned lambo store: {exc}"}
    finally:
        con.close()
    return {
        "concepts": concepts,
        "concepts_with_embedding": with_embedding,
        # Derive's `created` plus `record_action`'s: every fresh concept the
        # ledger saw. The store holds what survived. Positive shortfall = GC
        # swept, or the ledger dropped lines, or this store is not the one the
        # ledger was written beside.
        "ledger_created": created,
        "shortfall": created - concepts,
    }


def render(data: dict[str, Any]) -> list[str]:
    t = data["totals"]
    out = [
        f"{'bucket':<15} {'calls':>6} {'created':>8} {'matched':>8} "
        f"{'sem.merged':>11} {'dedup':>7}",
        "-" * 60,
    ]
    for row in data["buckets"]:
        r = row["dedup_rate"]
        out.append(
            f"{row['bucket']:<15} {row['calls']:>6} {row['created']:>8} "
            f"{row['matched']:>8} {row['semantic_merged']:>11} "
            f"{('n/a' if r is None else f'{r:.3f}'):>7}"
        )
    out.append("-" * 60)
    total_rate = "n/a" if t["dedup_rate"] is None else f"{t['dedup_rate']:.3f}"
    out.append(
        f"{'TOTAL':<15} {t['calls']:>6} {t['created']:>8} {t['matched']:>8} "
        f"{t['semantic_merged']:>11} {total_rate:>7}"
    )
    if data["failed_derive_calls"]:
        out.append(
            f"   ({data['failed_derive_calls']} derive call(s) failed and are excluded)"
        )
    joined = data["derive_facts_from_completion"]
    if joined:
        out.append(
            f"   {joined} derive call(s) were acknowledged asynchronously (J3) and their "
            "facts were JOINED from `completion` lines on the receipt (J4). The rates "
            "above include them. `sem.merged` is unavailable for those calls — the "
            "completion carries the created/matched pair and no more — so that column "
            "is an undercount, not a zero, whenever this number is non-zero."
        )
    if data["derive_calls_without_facts"]:
        out.append(
            f"   {data['derive_calls_without_facts']} SUCCESSFUL derive call(s) carried NO "
            "created/matched facts at all — not on the line, and no applied `completion` "
            "line joined on their receipt. Not the same as creating and matching nothing. "
            "Most likely the writes were acknowledged asynchronously (J3) by a serve that "
            "wrote no `completion` lines (a pre-J4 binary), in which case the facts are on "
            "the receipt and reachable with lambo_stats(receipt=...). Otherwise: the "
            "completion is in a ledger file this run did not read (pass every file), or the "
            "write is still owed a replay after a restart, or it failed after its ack, or a "
            "field was renamed. See the observability README's metric-2 note. "
            "The rates above are computed over the remaining calls only."
        )

    out += ["", "Per agent:"]
    for agent, a in data["agents"].items():
        r = a["dedup_rate"]
        out.append(
            f"   {agent[:24]:<24} {a['calls']:>5} calls  created={a['created']:<6} "
            f"matched={a['matched']:<6} dedup={'n/a' if r is None else f'{r:.3f}'}"
        )

    trend = [b["dedup_rate"] for b in data["buckets"] if b["dedup_rate"] is not None]
    if len(trend) >= 2:
        direction = "rising" if trend[-1] > trend[0] else (
            "falling" if trend[-1] < trend[0] else "flat"
        )
        out += [
            "",
            f"Trend across {len(trend)} bucket(s): {trend[0]:.3f} -> {trend[-1]:.3f} "
            f"({direction}). Rising means the vocabulary is converging.",
        ]

    store = data.get("store")
    if store:
        out += ["", "Store cross-check:"]
        if "error" in store:
            out.append(f"   {store['error']}")
        else:
            out.append(
                f"   store concepts={store['concepts']} "
                f"(with embedding {store['concepts_with_embedding']}), "
                f"ledger created={store['ledger_created']} "
                f"(derive {data['totals']['created']} + record_action "
                f"{data['record_action_created']}), "
                f"shortfall={store['shortfall']}"
            )
            if store["shortfall"] > 0:
                out.append(
                    "   ^ the store holds fewer concepts than the ledger created. "
                    "Usually a daemon GC sweep; also consistent with dropped "
                    "ledger lines or the wrong store path."
                )
            elif store["shortfall"] < 0:
                out.append(
                    "   ^ the store holds MORE than this ledger created: the "
                    "session predates the ledger, or another writer contributed."
                )
    return out


def main(argv: list[str]) -> int:
    parser = _ledger.base_parser(__doc__ or "")
    parser.add_argument(
        "--bucket",
        choices=sorted(BUCKETS),
        default="day",
        help="time axis for the trend (default: %(default)s)",
    )
    parser.add_argument(
        "--store",
        help="SQLite store path to cross-check the created total against",
    )
    args = parser.parse_args(argv)
    ledger = _ledger.load(args.ledger)
    data = analyse(ledger, args.bucket, args.store)
    _ledger.emit(
        "metric 2 — re-derivation savings", ledger, render(data), data, args.json
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
