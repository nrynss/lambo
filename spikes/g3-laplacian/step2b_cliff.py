"""G3 step 2b — is the depth cliff actually harmful?

Step 2 compared sets at equal budget. This asks the question the cliff is
accused of: does diffusion ever want to promote a node the cliff EXCLUDES
(directed distance >= 3) above a node the cliff ADMITS (distance 1 or 2)?

If the answer is "never", the cliff sits exactly where the diffusion decay
would put it and the two are the same gate expressed twice.

Also dumps the readable content of every set disagreement, for a human judge.
"""

import json
from collections import defaultdict, deque
from pathlib import Path

import numpy as np

from lambo_graph import TRAVERSAL_ORDER, LamboGraph
from step2_diffusion import (
    PPR_ALPHA,
    adjacency,
    heat_kernel,
    ledger_queries,
    ppr_matrix,
)

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"


def bfs_dist(adj, seeds, cap=12):
    """Directed BFS distance from a seed SET (min over seeds)."""
    dist = {s: 0 for s in seeds}
    dq = deque(seeds)
    while dq:
        x = dq.popleft()
        if dist[x] >= cap:
            continue
        for y in adj.get(x, ()):
            if y not in dist:
                dist[y] = dist[x] + 1
                dq.append(y)
    return dist


def main():
    g = LamboGraph()
    n = len(g.nodes)
    idx = g.index_of
    qs = ledger_queries()

    out_adj = defaultdict(list)
    for e in g.cc_edges:
        if e["type"] in TRAVERSAL_ORDER and e["source"] != e["target"]:
            out_adj[e["source"]].append(e["target"])

    A_dir = adjacency(g, typed=False, symmetric=False)
    A_und = adjacency(g, typed=False, symmetric=True)
    ops = {f"ppr_dir_a{a}": ppr_matrix(A_dir, a) for a in PPR_ALPHA}
    ops["heat_t1.0_und"] = heat_kernel(A_und, 1.0)
    ops["ppr_und_a0.3"] = ppr_matrix(A_und, 0.3)

    report = {}
    print("=== step 2b — cliff inversions ===\n")
    print("An INVERSION is a node at directed distance >= 3 (excluded by the")
    print("depth-2 cliff) that diffusion scores ABOVE some node at distance")
    print("1 or 2 (admitted by the cliff). Counted over all 22 real queries.\n")
    print(f"{'operator':<20} {'queries w/ inversion':>21} {'total inversions':>17} {'worst q':>9}")

    for name, M in ops.items():
        qwith, tot, worst = 0, 0, 0
        per_q = []
        for q in qs:
            p1 = g.phase1(q["query"], q["top_k"])
            seeds = [c for c, _, _ in p1]
            if not seeds:
                continue
            s = np.zeros(n)
            for cid, sc, _ in p1:
                s[idx[cid]] = sc
            s /= s.sum()
            v = M @ s
            dist = bfs_dist(out_adj, seeds)
            seedset = set(seeds)
            admitted = [
                x for x in g.nodes if x not in seedset and dist.get(x, 99) in (1, 2)
            ]
            far = [
                x
                for x in g.nodes
                if x not in seedset and dist.get(x, 99) >= 3 and v[idx[x]] > 1e-12
            ]
            if not admitted or not far:
                per_q.append(0)
                continue
            worst_admitted = min(v[idx[x]] for x in admitted)
            inv = [x for x in far if v[idx[x]] > worst_admitted]
            per_q.append(len(inv))
            if inv:
                qwith += 1
                tot += len(inv)
                worst = max(worst, len(inv))
        report[name] = dict(
            queries_with_inversion=qwith, total_inversions=tot, worst_query=worst
        )
        print(f"{name:<20} {f'{qwith}/{len(qs)}':>21} {tot:>17} {worst:>9}")

    # ---------------------------------------------------------- readable
    rows = json.loads((OUT / "h1_rows.json").read_text())
    print("\n=== the actual set disagreements, ppr_dir_a0.5 (the closest variant) ===")
    detail = []
    for r in rows:
        v = r["variants"]["ppr_dir_a0.5_plain"]
        if v["sym_diff"] == 0:
            continue
        print(f"\nquery: {r['query']!r}  (agent {r['agent']}, {r['n_seeds']} seeds)")
        print(f"  fixed-depth expansion: {r['n_fixed']} nodes; symmetric diff {v['sym_diff']}")
        d = dict(query=r["query"], added=[], dropped=[])
        for x in v["added"]:
            line = f"  + PPR admits (cliff excludes):  {g.snippet(x, 88)}"
            print(line)
            d["added"].append(g.snippet(x, 160))
        for x in v["dropped"]:
            lvl = r["fixed_levels"].get(x)
            line = f"  - cliff admits (lvl {lvl}), PPR drops: {g.snippet(x, 78)}"
            print(line)
            d["dropped"].append(dict(level=lvl, content=g.snippet(x, 160)))
        detail.append(d)

    print("\n=== ppr_dir_a0.15 disagreements (weakest decay, most aggressive) ===")
    for r in rows:
        v = r["variants"]["ppr_dir_a0.15_plain"]
        if v["sym_diff"] == 0:
            continue
        print(f"\nquery: {r['query']!r}  fixed={r['n_fixed']} symdiff={v['sym_diff']}")
        for x in v["added"]:
            print(f"  + {g.snippet(x, 88)}")
        for x in v["dropped"]:
            print(f"  - (lvl {r['fixed_levels'].get(x)}) {g.snippet(x, 84)}")

    (OUT / "h1_cliff.json").write_text(
        json.dumps(dict(inversions=report, disagreements=detail), indent=1)
    )
    print(f"\nwrote {OUT/'h1_cliff.json'}")


if __name__ == "__main__":
    main()
