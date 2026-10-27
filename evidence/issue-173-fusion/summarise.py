#!/usr/bin/env python3
"""Print the #173 fusion tables from the run files in runs/ (stdlib only)."""
import json
import pathlib

RUNS = pathlib.Path(__file__).parent / "runs"
SLICES = ["exact-identifier", "vocabulary-mismatch", "contrast", "fact-lookup", "rationale"]


def row(name, v, split):
    m = v["metrics"][f"split/{split}"]
    na = v["no_answer"].get(split, {})
    return (f"| `{name}` | {m['ndcg@10']:.3f} | {m['mrr@10']:.3f} | {m['hit@1']:.3f} | "
            f"{m['hit@5']:.3f} | {m['hit@10']:.3f} | {na.get('auc_top1', float('nan')):.3f} | "
            f"{gate if (gate := na.get('gate_pass_no_answer')) is not None else float('nan'):.3f} |")


def families(name, v, split):
    m = v["metrics"]
    cells = []
    for f in SLICES:
        a = m.get(f"{split}/family/{f}")
        cells.append(f"{a['ndcg@10']:.3f} ({a['n']})" if a else "—")
    return f"| `{name}` | " + " | ".join(cells) + " |"


HEAD = ("| variant | nDCG@10 | MRR@10 | hit@1 | hit@5 | hit@10 | no-answer AUC | "
        "no-answer past gate |\n|---|---|---|---|---|---|---|---|")
FHEAD = "| variant | " + " | ".join(SLICES) + " |\n|---|" + "---|" * len(SLICES)

for model in ["bge-m3-candle-cpu", "eg2-q8-768"]:
    for kind, split in [("dev", "dev"), ("confirm", "held_out")]:
        path = RUNS / f"{model}-{kind}.json"
        if not path.exists():
            continue
        d = json.loads(path.read_text())
        print(f"\n### {model}, {split}\n\n{HEAD}")
        for name, v in d["variants"].items():
            print(row(name, v, split))
        print(f"\nnDCG@10 by query family (n), {split}:\n\n{FHEAD}")
        for name, v in d["variants"].items():
            if kind == "confirm" or name in ("max", "rrf:k=60", "rrf:k=3"):
                print(families(name, v, split))
