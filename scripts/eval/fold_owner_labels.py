#!/usr/bin/env python3
"""#190 owner tool: fold labelled sheets from sample_store_pairs.py and
ledger_misses.py into fixtures/eval/authoring/owner_{pairs,queries}.json,
which build.py picks up. Rows marked `private`, `skip` or unlabelled are
dropped and never written. Then rerun build.py and the eval_set test.

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
        (OUT / "owner_pairs.json").write_text(json.dumps(keep, indent=1, ensure_ascii=False) + "\n")
        print(f"pairs: kept {len(keep)} of {len(rows)}")
    if a.queries:
        rows = json.loads(Path(a.queries).read_text())
        keep = [{"query": r["query"], "labeller": a.labeller, "labelled_on": a.date, "note": r.get("note")}
                for r in rows if r.get("label") == "no_answer" and r.get("private") is False]
        (OUT / "owner_queries.json").write_text(json.dumps(keep, indent=1, ensure_ascii=False) + "\n")
        print(f"no-answer queries: kept {len(keep)} of {len(rows)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
