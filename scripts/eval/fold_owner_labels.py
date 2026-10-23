#!/usr/bin/env python3
"""#190 owner tool: fold labelled sheets from sample_store_pairs.py and
ledger_misses.py into fixtures/eval/authoring/owner_{pairs,queries}.json,
which build.py picks up. Rows marked `private`, `skip` or unlabelled are
dropped and never written. Folding another sheet merges into what is there:
a row for the same two texts (pairs) or the same query replaces the earlier
one, and every other earlier row is kept. Then rerun build.py and the
eval_set test.

    python3 scripts/eval/fold_owner_labels.py --pairs sheet.json --queries misses.json \
        --labeller "<name>" --date <YYYY-MM-DD>
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "fixtures/eval/authoring"
LABELS = {"related_distinct", "paraphrase", "unrelated"}


def merge(path: Path, rows: list, key) -> int:
    """Write the earlier rows of `path` plus `rows`, a new row replacing an
    earlier one with the same key. Returns the total written."""
    merged = {key(r): r for r in (json.loads(path.read_text()) if path.exists() else [])}
    for r in rows:
        merged[key(r)] = r
    out = list(merged.values())
    path.write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
    return len(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pairs")
    ap.add_argument("--queries")
    ap.add_argument("--labeller", required=True)
    ap.add_argument("--date", required=True)
    a = ap.parse_args()
    if a.pairs:
        rows = json.loads(Path(a.pairs).read_text())
        keep = [{"id": r["id"], "a": r["a"], "b": r["b"], "label": r["label"], "family": r["family"],
                 "relation": r.get("relation"), "labeller": a.labeller, "labelled_on": a.date,
                 "note": r.get("note")}
                for r in rows if r.get("label") in LABELS and r.get("private") is False]
        total = merge(OUT / "owner_pairs.json", keep, lambda r: tuple(sorted((r["a"], r["b"]))))
        print(f"pairs: kept {len(keep)} of {len(rows)}; {total} owner pairs in all")
    if a.queries:
        rows = json.loads(Path(a.queries).read_text())
        keep = [{"query": r["query"], "labeller": a.labeller, "labelled_on": a.date, "note": r.get("note")}
                for r in rows if r.get("label") == "no_answer" and r.get("private") is False]
        total = merge(OUT / "owner_queries.json", keep, lambda r: r["query"])
        print(f"no-answer queries: kept {len(keep)} of {len(rows)}; {total} in all")
    return 0


if __name__ == "__main__":
    sys.exit(main())
