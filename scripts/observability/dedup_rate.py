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
`src/graph/derive.rs`. `reinforced` sits beside it for the same reason.

**Both come through the join since #12 (0.4.0), and only when present.** An
applied completion line from a 0.4.0+ serve carries `semantic_merged` and
`reinforced` for a derive; one from an older serve carries the created/matched
pair and no more. An absent key is "not available", never zero, so each of the
two columns is computed over the derive calls that HAVE facts (line-carried or
joined; fact-less calls are excluded here exactly as they are from the rate)
under one rule, per bucket, per agent and in TOTAL:

  * no contributing call carried the key -> `n/a` (JSON `null`), never `0`;
  * every contributing call carried it   -> the exact sum;
  * some did and some did not (a ledger spanning the 0.4.0 upgrade, or a
    bucket mixing old and new serves) -> the sum of what is present, rendered
    with a trailing `+` because it is a LOWER BOUND. The JSON keeps the sum in
    `semantic_merged` / `reinforced` and counts the calls that lacked the key
    in `semantic_merged_missing` / `reinforced_missing`, so `null`, an exact
    figure and a partial one are three distinguishable states.

With `--store <path>` the ledger's own arithmetic is checked against the SQLite
store: created-minus-matched should land near the store's concept count. A large
shortfall is normally the daemon's GC having swept unreferenced concepts (the
C1–C3 capture saw exactly this), not a broken ledger — but it should be *seen*
rather than assumed either way. The same flag counts the store's `Semantic`
edges whose `created_at` falls inside the ledger's derive window, the store-side
figure `sem.merged` should land near (approximate: an edge upserted again keeps
only its latest `created_at`, and GC may have swept some).

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

#: The derive facts that make the dedup rate. Every producer writes them as a
#: pair; a call carrying neither is fact-less and reported on its own line.
RATE_FACTS = ("created", "matched")

#: The derive facts reported beside the rate and deliberately not folded in.
#: Each may be absent on a call that has the rate facts (a completion line from
#: a pre-#12 serve), so each is summed under the present / partial / n/a rule in
#: the module docstring rather than defaulted to zero.
OPTIONAL_FACTS = ("semantic_merged", "reinforced")


def _new_row() -> dict[str, int]:
    row = {"calls": 0, "created": 0, "matched": 0}
    for name in OPTIONAL_FACTS:
        # `name` is the running sum; `_seen` / `_missing` count the fact-bearing
        # calls that did / did not carry the key. Finalised by `_finish`.
        row[name] = 0
        row[f"{name}_seen"] = 0
        row[f"{name}_missing"] = 0
    return row


def _add(row: dict[str, int], facts: dict[str, int]) -> None:
    for name in RATE_FACTS:
        row[name] += facts.get(name, 0)
    for name in OPTIONAL_FACTS:
        value = facts.get(name)
        if isinstance(value, int):
            row[name] += value
            row[f"{name}_seen"] += 1
        else:
            row[f"{name}_missing"] += 1


def _finish(row: dict[str, int]) -> dict[str, Any]:
    """Apply the availability rule: `null` when no contributing call carried
    the key, otherwise the sum (partial when `<name>_missing` is non-zero)."""
    out: dict[str, Any] = {k: v for k, v in row.items() if not k.endswith("_seen")}
    for name in OPTIONAL_FACTS:
        if not row[f"{name}_seen"]:
            out[name] = None
    return out


def _merge(rows: list[dict[str, int]]) -> dict[str, int]:
    total = _new_row()
    for row in rows:
        for k in total:
            total[k] += row[k]
    return total


def analyse(ledger: _ledger.Ledger, bucket: str, store: str | None) -> dict[str, Any]:
    width = BUCKETS[bucket]
    per_bucket: dict[str, dict[str, int]] = defaultdict(_new_row)
    per_agent: dict[str, dict[str, int]] = defaultdict(_new_row)
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
    # The ledger window the store's `Semantic` edges are counted over: first to
    # last successful derive, whatever its facts.
    window: list[str] = []
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
        window = [window[0] if window else call["ts"], call["ts"]]
        key = call["ts"][:width] if width else "all"
        row = per_bucket[key]
        row["calls"] += 1
        agent_row = per_agent[call.get("agent_id", "?")]
        agent_row["calls"] += 1
        facts, source = _ledger.joined_facts(call, completions)
        if source == "none":
            # Excluded from every fact column, the optional two included: a
            # call this file has no outcome for is not "missing semantic_merged",
            # it is missing everything, and is reported once, below.
            factless += 1
            continue
        _add(row, facts)
        _add(agent_row, facts)
        if source == "completion":
            from_completion += 1
        else:
            from_line += 1

    def rate(row: dict[str, Any]) -> float | None:
        total = row["created"] + row["matched"]
        return (row["matched"] / total) if total else None

    buckets = [
        {"bucket": k, **_finish(v), "dedup_rate": rate(v)}
        for k, v in sorted(per_bucket.items())
    ]
    totals = _merge(list(per_bucket.values()))

    data: dict[str, Any] = {
        "bucket": bucket,
        "buckets": buckets,
        "agents": {
            a: {**_finish(v), "dedup_rate": rate(v)} for a, v in sorted(per_agent.items())
        },
        "totals": {**_finish(totals), "dedup_rate": rate(totals)},
        "failed_derive_calls": failed,
        "derive_calls_without_facts": factless,
        "derive_facts_from_line": from_line,
        "derive_facts_from_completion": from_completion,
        "record_action_created": action_created,
    }

    if store:
        data["store"] = store_check(
            store, totals["created"] + action_created, tuple(window) or None
        )
    return data


def _store_ts(raw: str) -> Any:
    """The store writes `...Z` (chrono `SecondsFormat::Millis`); `parse_ts`
    needs an explicit offset on Pythons before 3.11."""
    if raw.endswith("Z"):
        raw = raw[:-1] + "+00:00"
    return _ledger.parse_ts(raw)


def store_check(
    path: str, created: int, window: tuple[str, ...] | None
) -> dict[str, Any]:
    """Compare every concept the ledger saw created against the store's count,
    and count the `Semantic` edges created inside the ledger's derive window."""
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
        con.close()
        return {"error": f"{path} does not look like a provisioned lambo store: {exc}"}
    # `Semantic` edges: optional, so a store without an `edges` table (or a
    # ledger with no successful derive to bound a window) reports `null` rather
    # than zero. `edge_type` is the serde PascalCase name `enum_to_text` writes.
    semantic: dict[str, Any] = {"semantic_edges": None, "semantic_edges_in_window": None}
    try:
        stamps = [
            r[0]
            for r in con.execute(
                "SELECT created_at FROM edges WHERE edge_type = 'Semantic'"
            ).fetchall()
        ]
    except sqlite3.Error as exc:
        semantic["semantic_edges_error"] = f"could not read Semantic edges: {exc}"
    else:
        semantic["semantic_edges"] = len(stamps)
        if window:
            lo, hi = _ledger.parse_ts(window[0]), _ledger.parse_ts(window[-1])
            inside = 0
            for raw in stamps:
                try:
                    when = _store_ts(raw)
                except (TypeError, ValueError):
                    continue
                if lo <= when <= hi:
                    inside += 1
            semantic["semantic_edges_in_window"] = inside
            semantic["window"] = [window[0], window[-1]]
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
        **semantic,
    }


def _optional(row: dict[str, Any], name: str) -> str:
    """Render one optional fact under the module docstring's rule."""
    value = row[name]
    if value is None:
        return "n/a"
    return f"{value}+" if row[f"{name}_missing"] else str(value)


def render(data: dict[str, Any]) -> list[str]:
    t = data["totals"]
    rule = "-" * 70
    out = [
        f"{'bucket':<15} {'calls':>6} {'created':>8} {'matched':>8} "
        f"{'sem.merged':>11} {'reinf.':>7} {'dedup':>7}",
        rule,
    ]
    for row in data["buckets"]:
        r = row["dedup_rate"]
        out.append(
            f"{row['bucket']:<15} {row['calls']:>6} {row['created']:>8} "
            f"{row['matched']:>8} {_optional(row, 'semantic_merged'):>11} "
            f"{_optional(row, 'reinforced'):>7} "
            f"{('n/a' if r is None else f'{r:.3f}'):>7}"
        )
    out.append(rule)
    total_rate = "n/a" if t["dedup_rate"] is None else f"{t['dedup_rate']:.3f}"
    out.append(
        f"{'TOTAL':<15} {t['calls']:>6} {t['created']:>8} {t['matched']:>8} "
        f"{_optional(t, 'semantic_merged'):>11} {_optional(t, 'reinforced'):>7} "
        f"{total_rate:>7}"
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
            "above include them."
        )
    with_facts = data["derive_facts_from_line"] + joined
    for name, label in (("semantic_merged", "sem.merged"), ("reinforced", "reinf.")):
        missing = t[f"{name}_missing"]
        if not with_facts or not missing:
            continue
        if t[name] is None:
            out.append(
                f"   `{label}` is n/a: none of the {missing} derive call(s) with facts "
                f"carried `{name}` (completion lines from a serve before #12 / 0.4.0 "
                "carry created/matched and no more). Unavailable, not zero."
            )
        else:
            out.append(
                f"   `{label}` with a trailing + is a LOWER BOUND, an undercount, not a "
                f"zero: {missing} derive call(s) with facts carried no `{name}` "
                "(completion lines from a serve before #12 / 0.4.0), and only the "
                "calls that did are summed."
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
            f"matched={a['matched']:<6} sem.merged={_optional(a, 'semantic_merged'):<5} "
            f"reinf.={_optional(a, 'reinforced'):<5} "
            f"dedup={'n/a' if r is None else f'{r:.3f}'}"
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
            if store.get("semantic_edges_error"):
                out.append(f"   {store['semantic_edges_error']}")
            elif store.get("semantic_edges_in_window") is None:
                out.append(
                    f"   store Semantic edges={store['semantic_edges']} "
                    "(no successful derive in the ledger to bound a window)"
                )
            else:
                lo, hi = store["window"]
                out.append(
                    f"   store Semantic edges created {lo} .. {hi}: "
                    f"{store['semantic_edges_in_window']} "
                    f"(of {store['semantic_edges']} in the store), ledger sem.merged="
                    f"{_optional(t, 'semantic_merged')}"
                )
                out.append(
                    "   ^ should be in the same ballpark. Approximate both ways: a "
                    "re-upserted edge keeps only its latest created_at, GC sweeps "
                    "edges, and `n/a` / `+` above mean the ledger side is unknown "
                    "or a lower bound."
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
