#!/usr/bin/env python3
"""Group per-agent usage rows (from extract.py) into work items and roles.

Usage: categorize.py <rows.jsonl> [<public-rows.jsonl>]

Prints the markdown tables to stdout and the per-agent mapping to stderr.
With a second argument, also writes the rows without the fields that can hold
local paths or prompt text (first_prompt, agent ids), with the work item and
role added, for publishing.
"""
import json
import re
import sys
from collections import defaultdict

ITEMS = [
    # (work item, regex on description), first match wins
    ("Comment ID cleanup (not Lean)", r"^Strip IDs|^Sweep tracker IDs|^Label sweep"),
    ("Integration of the fix wave", r"^Integrate theorems|integration branch"),
    ("#288 model-first", r"#288"),
    ("Modelling wave (13 models)", r"Lean lane|^Re-review drain remediation"),
    ("State-machine models (#273)", r"^Model |Review (BatchQueue|Registry|Tiered|Resolve) Lean model"),
    ("Fix wave #293–#302", r"#29[3-9]|#30[0-2]"),
    ("Other fixes (#315, #319, #322)", r"#315|#319|#322"),
    ("#325 model-first", r"#325"),
    ("#291 drift check", r"(?i)drift"),
    ("#291 constants", r"constants|Constants"),
    ("#291 property tests", r"property-test|property tests|canon cycle replay"),
    ("#291 differential oracle (found #328)", r"oracle|#328"),
    ("#287 discovery race", r"#287"),
    ("Release 0.15.0 and gate evidence", r"0\.15\.0"),
]


def item(desc):
    for name, rx in ITEMS:
        if re.search(rx, desc or ""):
            return name
    return "Unclassified"


def role(desc):
    d = desc or ""
    if re.match(r"(Final )?[Rr]e-review|Review", d):
        return "review"
    if re.match(r"Remediate|.*round-\d fixes|.*round-3 fixes", d):
        return "remediation"
    return "implementation"


def main():
    rows = [json.loads(l) for l in open(sys.argv[1])]
    agg = defaultdict(lambda: defaultdict(float))
    roles = defaultdict(lambda: defaultdict(float))
    per_issue = defaultdict(lambda: defaultdict(float))
    order = [n for n, _ in ITEMS] + ["Unclassified"]
    public = []
    for r in rows:
        if r["kind"] != "subagent":
            public.append({k: v for k, v in r.items() if k not in ("first_prompt", "agent")})
            continue
        it = item(r["description"])
        ro = role(r["description"])
        public.append({**{k: v for k, v in r.items() if k not in ("first_prompt", "agent")},
                       "work_item": it, "role": ro})
        for tgt in (agg[it], roles[ro]):
            tgt["agents"] += 1
            tgt["messages"] += r["messages"]
            tgt["fresh_in"] += r["input_tokens"] + r["cache_creation_input_tokens"]
            tgt["cache_read"] += r["cache_read_input_tokens"]
            tgt["output"] += r["output_tokens"]
            tgt["active_s"] += r["active_s"]
        agg[it][ro + "_agents"] += 1
        if it == "Fix wave #293–#302":
            key = ", ".join(sorted(set(re.findall(r"#\d+", r["description"]))))
            per_issue[key]["agents"] += 1
            per_issue[key]["output"] += r["output_tokens"]
            per_issue[key]["fresh_in"] += r["input_tokens"] + r["cache_creation_input_tokens"]
            per_issue[key]["cache_read"] += r["cache_read_input_tokens"]
            per_issue[key]["active_s"] += r["active_s"]
        print(f"{it:<42} {ro:<15} {r['description']}", file=sys.stderr)

    def line(name, a):
        return (f"| {name} | {int(a['agents'])} | {a['fresh_in']/1e6:.2f} | {a['cache_read']/1e6:.1f} | "
                f"{a['output']/1e3:,.0f} | {a['active_s']/3600:.1f} |")

    hdr = "| {} | Agents | Fresh input (M) | Cache read (M) | Output (k) | Agent-hours active |\n|---|---:|---:|---:|---:|---:|"
    print(hdr.format("Work item").replace("active |\n|---|---:|---:|---:|---:|---:|", "active | Impl / review / remediation agents |\n|---|---:|---:|---:|---:|---:|---|"))
    tot = defaultdict(float)
    for n in order:
        if n in agg:
            a = agg[n]
            print(line(n, a) + f" {int(a['implementation_agents'])} / {int(a['review_agents'])} / {int(a['remediation_agents'])} |")
            for k, v in a.items():
                tot[k] += v
    print(line("**All subagents**", tot) + " |")
    main = next(r for r in rows if r["kind"] == "main")
    m = defaultdict(float, agents=1, fresh_in=main["input_tokens"] + main["cache_creation_input_tokens"],
                    cache_read=main["cache_read_input_tokens"], output=main["output_tokens"], active_s=main["active_s"])
    print(line("Main session (orchestration, all work)", m) + " |")
    print()
    print(hdr.format("Role"))
    for n in ("implementation", "review", "remediation"):
        print(line(n, roles[n]))
    print()
    print(hdr.format("Fix-wave issue"))
    for k in sorted(per_issue):
        print(line(k, per_issue[k]))
    if len(sys.argv) > 2:
        with open(sys.argv[2], "w") as f:
            for r in public:
                f.write(json.dumps(r, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
