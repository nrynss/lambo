#!/usr/bin/env python3
"""Collect the dogfood-store texts that are already public into
dogfood_texts.json, with provenance per text.

Sources (all public, all already through a hygiene pass):
  * evidence/issue-178-similar/labelled-pairs.json, rows with source dogfood-i3
  * evidence/mooshik-i3-live-ledger/duplicates.txt (printed pair texts)
  * the published, redacted nrynss/lambo-paper copy of the 2026-09-07 runs,
    lambo run only (the vimanam run's corpus is another project), full
    recall exports *.full.json (calls.jsonl is skipped: it carries a host name)

A text carrying a redaction marker is dropped, never partially kept.

    python3 extract_dogfood_texts.py --paper /path/to/lambo-paper
"""
import argparse, json, re, subprocess, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
HIT = re.compile(r"^(.*) \[(\w+)\] \(score [\d.]+\)$", re.S)
DUP = re.compile(r"^\s+(?:\d\.\d{4}\s+)?(['\"])(.*)\1\s*$")


def add(out, text, src, typ=None):
    text = text.strip()
    if not text or "[REDACTED" in text:
        return
    e = out.setdefault(text, {"text": text, "sources": [], "type": typ})
    if src not in e["sources"]:
        e["sources"].append(src)
    if typ and not e["type"]:
        e["type"] = typ


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--paper", required=True)
    a = ap.parse_args()
    out = {}
    pairs = json.loads((ROOT / "evidence/issue-178-similar/labelled-pairs.json").read_text(encoding="utf-8"))
    for p in pairs:
        if p["source"] == "dogfood-i3":
            add(out, p["a"], "evidence/issue-178-similar/labelled-pairs.json")
            add(out, p["b"], "evidence/issue-178-similar/labelled-pairs.json")
    for line in (ROOT / "evidence/mooshik-i3-live-ledger/duplicates.txt").read_text(encoding="utf-8").splitlines():
        m = DUP.match(line)
        if m:
            add(out, m.group(2), "evidence/mooshik-i3-live-ledger/duplicates.txt")
    paper = Path(a.paper)
    rev = subprocess.run(["git", "-C", str(paper), "rev-parse", "--short", "HEAD"],
                         capture_output=True, encoding="utf-8", check=True).stdout.strip()
    run = paper / "evaluation/results/2026-09-07/lambo-2026-09-07/exports/lambo"
    for f in sorted(run.glob("*.full.json")):
        ctx = json.loads(f.read_text(encoding="utf-8"))["structuredContent"]["context"]
        for chunk in ctx.split("\n\n"):
            m = HIT.match(chunk.strip())
            if m:
                rel = f.relative_to(paper)
                add(out, m.group(1), f"nrynss/lambo-paper@{rev}:{rel}", m.group(2))
    rows = sorted(out.values(), key=lambda e: e["text"])
    # The texts are not ASCII; write UTF-8 whatever the locale says.
    sys.stdout.reconfigure(encoding="utf-8")
    json.dump(rows, sys.stdout, indent=1, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
