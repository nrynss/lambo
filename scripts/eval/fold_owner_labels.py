#!/usr/bin/env python3
"""#190 owner tool: fold labelled sheets from sample_store_pairs.py and
ledger_misses.py into fixtures/eval/authoring/owner_{pairs,queries}.json,
which build.py picks up. Rows marked `private`, `skip` or unlabelled are
dropped and never written. Folding another sheet merges into what is there:
a kept row for the same two texts (pairs) or the same query replaces the
earlier one, a dropped row withdraws the earlier one (so marking a folded
row private or skip later takes it out), and every row the sheet does not
mention is kept. Then rerun build.py and the eval_set test.

    python3 scripts/eval/fold_owner_labels.py --pairs sheet.json --queries misses.json \
        --labeller "<name>" --date <YYYY-MM-DD>
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "fixtures/eval/authoring"
# Mirrors examples/eval_measure/schema.rs, so a bad row fails here, before
# build.py writes a set the schema rejects.
FAMILIES = {
    "related_distinct": {"identifier", "quantity", "polarity", "repository",
                         "correction", "exception", "topic-neighbour"},
    "paraphrase": {"rewording", "terse", "near-verbatim", "same-event"},
    "unrelated": {"cross-area"},
}
LABELS = set(FAMILIES)
RELATIONS = {"supersedes", "distinct_entity", "contradicts"}
RELATION_REQUIRED = {"identifier", "correction"}


def pair_problem(r: dict) -> str | None:
    family, relation = r.get("family"), r.get("relation")
    if family not in FAMILIES[r["label"]]:
        return f"family {family!r} is not one of {sorted(FAMILIES[r['label']])}"
    if relation is not None and r["label"] != "related_distinct":
        return "only related_distinct pairs carry a relation"
    if relation is not None and relation not in RELATIONS:
        return f"relation {relation!r} is not one of {sorted(RELATIONS)}"
    if family in RELATION_REQUIRED and relation is None:
        return f"family {family} needs a relation"
    return None


def pair_id(a: str, b: str) -> str:
    """A pair id from its two texts, whatever their order: sheet ids
    (store-0001, ...) restart in every sheet, so they cannot be the id."""
    return "store-" + hashlib.sha256("\x00".join(sorted((a, b))).encode()).hexdigest()[:10]


def merge(path: Path, rows: list, withdrawn: set, key) -> tuple[int, int]:
    """Write the earlier rows of `path` plus `rows`: a new row replaces an
    earlier one with the same key, and an earlier row whose key is in
    `withdrawn` is removed and not re-added. Returns (total written, rows removed)."""
    merged = {key(r): r for r in (json.loads(path.read_text(encoding="utf-8")) if path.exists() else [])}
    removed = [k for k in withdrawn if merged.pop(k, None) is not None]
    # A key the sheet both keeps and withdraws (a repeated row) stays out.
    for r in rows:
        if key(r) not in withdrawn:
            merged[key(r)] = r
    out = list(merged.values())
    path.write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    return len(out), len(removed)


def pair_key(r: dict) -> tuple[str, str]:
    return tuple(sorted((r["a"], r["b"])))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pairs")
    ap.add_argument("--queries")
    ap.add_argument("--labeller", required=True)
    ap.add_argument("--date", required=True)
    a = ap.parse_args()
    if a.pairs:
        rows = json.loads(Path(a.pairs).read_text(encoding="utf-8"))
        eligible = [r for r in rows if r.get("label") in LABELS and r.get("private") is False]
        bad = [f"{r.get('id', '?')}: {p}" for r in eligible if (p := pair_problem(r))]
        if bad:
            sys.exit("fix these rows and fold again (nothing was written):\n" + "\n".join(bad))
        keep = [{"id": pair_id(r["a"], r["b"]), "a": r["a"], "b": r["b"], "label": r["label"], "family": r["family"],
                 "relation": r.get("relation"), "labeller": a.labeller, "labelled_on": a.date,
                 "note": r.get("note")}
                for r in eligible]
        withdrawn = {pair_key(r) for r in rows
                     if r not in eligible and isinstance(r.get("a"), str) and isinstance(r.get("b"), str)}
        total, removed = merge(OUT / "owner_pairs.json", keep, withdrawn, pair_key)
        print(f"pairs: kept {len(keep)} of {len(rows)}; withdrew {removed}; {total} owner pairs in all")
    if a.queries:
        rows = json.loads(Path(a.queries).read_text(encoding="utf-8"))
        eligible = [r for r in rows if r.get("label") == "no_answer" and r.get("private") is False]
        keep = [{"query": r["query"], "labeller": a.labeller, "labelled_on": a.date, "note": r.get("note")}
                for r in eligible]
        withdrawn = {r["query"] for r in rows if r not in eligible and isinstance(r.get("query"), str)}
        total, removed = merge(OUT / "owner_queries.json", keep, withdrawn, lambda r: r["query"])
        print(f"no-answer queries: kept {len(keep)} of {len(rows)}; withdrew {removed}; {total} in all")
    return 0


if __name__ == "__main__":
    sys.exit(main())
