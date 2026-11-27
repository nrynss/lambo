#!/usr/bin/env python3
"""Per-query top-8 comparison behind the A/B's `recall_topk_jaccard`.

The A/B harness reports only the aggregate. This asks the same 20 queries, in the
same order and with the same warm-up, of three serves: two instances of the
baseline build (a same-build control) and one of the candidate. It prints
which queries differ between baseline and candidate, and whether the two
baseline instances differ from each other on the same queries.

The printed report holds counts, id prefixes, scores, each hit's concept type
and session name, and the first 110 characters of up to four warning lines
per side. A warning line can quote memory text or name an agent, so read the
report before committing it; in the 0.15.0 gate every printed warning was a
"Session inactive N seconds" line or a "Load-bearing pillar" line that gives
only a count of dependent nodes. The file given to --save holds the full
recall bodies, which include memory content: keep it out of the repository.

Usage:
    python3 probe_recall.py --side 0150a=URL --side 0150b=URL --side 0160=URL \
        [--rounds 3] --save bodies.json > probe-recall.txt

This is the Metal gate's script, retagged to 0150/0160, with the harness
import below relative to this file.
"""
import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "scripts" / "loadtest"))
import mcp_ab  # noqa: E402


def jaccard(x, y):
    x, y = set(x), set(y)
    return 1.0 if not (x or y) else len(x & y) / len(x | y)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--side", action="append", required=True, help="name=url, three times")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--save", required=True)
    o = ap.parse_args()
    sides = {}
    for spec in o.side:
        name, url = spec.split("=", 1)
        sides[name] = mcp_ab.Mcp(url)
    names = list(sides)
    for name, c in sides.items():  # same warm-up as mcp_ab
        for q in mcp_ab.QUERIES[:3]:
            c.tool("lambo_recall", query=q)
    bodies = {n: {} for n in names}  # name -> query -> [body per round]
    for _ in range(o.rounds):
        for q in mcp_ab.QUERIES:
            for name, c in sides.items():
                _, body, err = c.tool("lambo_recall", query=q, top_k=8)
                bodies[name].setdefault(q, []).append({"err": bool(err), "body": body})
    with open(o.save, "w") as f:
        json.dump(bodies, f)

    def ids(name, q, rnd=0):
        return [h["node_id"] for h in bodies[name][q][rnd]["body"].get("hits", [])]

    base, ctrl, cand = names[0], names[1], names[2]
    print(f"sides: baseline={base} control={ctrl} candidate={cand}; rounds={o.rounds}")
    print(f"{'q':>3} {'n_base':>6} {'n_ctrl':>6} {'n_cand':>6} {'J base/cand':>12} {'J base/ctrl':>12} {'stable b/c/n':>13}")
    tot_bc = tot_bb = 0.0
    differing = []
    for i, q in enumerate(mcp_ab.QUERIES):
        jb = jaccard(ids(base, q), ids(cand, q))
        jc = jaccard(ids(base, q), ids(ctrl, q))
        tot_bc += jb
        tot_bb += jc
        stable = "/".join(
            "y" if all(set(ids(n, q, r)) == set(ids(n, q, 0)) for r in range(o.rounds)) else "N"
            for n in (base, ctrl, cand))
        print(f"{i:>3} {len(ids(base, q)):>6} {len(ids(ctrl, q)):>6} {len(ids(cand, q)):>6} {jb:>12.2f} {jc:>12.2f} {stable:>13}")
        if jb < 1.0 or jc < 1.0:
            differing.append(i)
    n = len(mcp_ab.QUERIES)
    print(f"mean Jaccard baseline/candidate: {tot_bc / n:.3f}   baseline/control (same build): {tot_bb / n:.3f}")

    for i in differing:
        q = mcp_ab.QUERIES[i]
        print(f"\n--- query {i}: first-round hits per side (id prefix, score, type, session, warnings)")
        for nme in names:
            body = bodies[nme][q][0]["body"]
            print(f"  [{nme}] err={bodies[nme][q][0]['err']} hits={len(body.get('hits', []))} "
                  f"warnings={len(body.get('warnings', []) or [])} top_legs={body.get('top_legs')}")
            for w in (body.get("warnings") or [])[:4]:
                print(f"      warning: {str(w)[:110]}")
            for rank, h in enumerate(body.get("hits", [])):
                others = [x for x in names if x != nme]
                mark = "".join("=" if h["node_id"] in ids(x, q) else "!" for x in others)
                print(f"      {rank} {h['node_id'][:8]} score={h.get('score', 0):.4f} {h.get('concept_type')} "
                      f"session={h.get('session')} in-others={mark} companion={bool(h.get('companion_of'))}")


if __name__ == "__main__":
    main()
