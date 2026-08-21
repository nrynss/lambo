"""G3 step 3b — H2's confound control.

Step 3 found the resistance-ranked warning set overlaps the product's
`blast_radius > 5` set only 4/9. But `blast_radius` filters to EXCLUSIVE
dependents while conductance sums over ALL structural dependents, so the
disagreement has two candidate causes:

  (a) resistance / path multiplicity — H2's actual claim, or
  (b) merely counting all dependents instead of only the exclusive ones,
      which needs no Laplacian at all.

The control: rank by `count_all` (a plain count, no resistance) and see how much
of the reordering it already explains. Whatever conductance adds ON TOP of
count_all is H2's real contribution.

Also isolates the spec's exact scenario — a dependent held by five independent
paths vs one held by a single chain — and reports whether the dogfood graph
contains any instance of it.
"""

import json
from collections import defaultdict
from pathlib import Path

import numpy as np

from lambo_graph import STRUCTURAL, LamboGraph

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
MIN_BLAST = 5


def main():
    g = LamboGraph()
    h2 = json.loads((OUT / "h2.json").read_text())
    recs = {r["node"]: r for r in h2["records"]}

    def top(key, k):
        return [
            r["node"]
            for r in sorted(recs.values(), key=lambda r: (-r[key], r["node"]))[:k]
        ]

    warn_now = set(h2["warn_now"])
    k = len(warn_now)
    t_all = set(top("count_all", k))
    t_cond = set(top("conductance", k))

    def j(a, b):
        return len(a & b) / len(a | b)

    print("=== step 3b — attributing H2's disagreement ===\n")
    print(f"warning set size k = {k} (blast_radius > {MIN_BLAST})\n")
    print(f"{'comparison':<44} {'overlap':>9} {'Jaccard':>8}")
    print(
        f"{'blast>5  vs  count_all top-k    (CONTROL)':<44} "
        f"{len(warn_now & t_all)}/{k:<7} {j(warn_now, t_all):>8.3f}"
    )
    print(
        f"{'blast>5  vs  conductance top-k  (H2)':<44} "
        f"{len(warn_now & t_cond)}/{k:<7} {j(warn_now, t_cond):>8.3f}"
    )
    print(
        f"{'count_all top-k vs conductance top-k (H2 net)':<44} "
        f"{len(t_all & t_cond)}/{k:<7} {j(t_all, t_cond):>8.3f}"
    )
    print()
    print("Reading: if row 1 ~= row 2 and row 3 ~= 1.0, the reordering is the")
    print("exclusivity filter, not the resistance, and no Laplacian is needed.")
    print()

    # Kendall tau over the full 65-concept set
    def tau(ka, kb):
        a = sorted(recs.values(), key=lambda r: (-r[ka], r["node"]))
        b = sorted(recs.values(), key=lambda r: (-r[kb], r["node"]))
        ra = {r["node"]: i for i, r in enumerate(a)}
        rb = {r["node"]: i for i, r in enumerate(b)}
        ns = list(ra)
        c = d = 0
        for i in range(len(ns)):
            for jj in range(i + 1, len(ns)):
                x, y = ns[i], ns[jj]
                s = (ra[x] - ra[y]) * (rb[x] - rb[y])
                if s > 0:
                    c += 1
                elif s < 0:
                    d += 1
        return (c - d) / (c + d) if (c + d) else None

    print("Kendall tau over all 65 concepts with a structural dependent:")
    print(f"  count_exclusive vs count_all    {tau('count_exclusive','count_all'):+.3f}")
    print(f"  count_exclusive vs conductance  {tau('count_exclusive','conductance'):+.3f}")
    print(f"  count_all       vs conductance  {tau('count_all','conductance'):+.3f}  <-- H2 net")
    print()

    # Does conductance ever disagree with count_all on ORDER within the top?
    a = sorted(recs.values(), key=lambda r: (-r["count_all"], r["node"]))[:14]
    b = sorted(recs.values(), key=lambda r: (-r["conductance"], r["node"]))[:14]
    print("top-14 by count_all vs by conductance, side by side:")
    print(f"{'#':>3}  {'all':>4} {'cond':>7}  content(count_all order)")
    for i, r in enumerate(a):
        print(f"{i+1:>3}  {r['count_all']:>4} {r['conductance']:>7.2f}  {r['content'][:62]}")
    print()
    same = [x["node"] for x in a] == [x["node"] for x in b]
    print(f"identical top-14 order? {same}")
    moved = [
        (x["node"], i, [y["node"] for y in b].index(x["node"]))
        for i, x in enumerate(a)
        if x["node"] in {y["node"] for y in b}
    ]
    big = [(n, i, k2) for n, i, k2 in moved if abs(i - k2) >= 3]
    print(f"members moving >=3 places between count_all and conductance: {len(big)}")
    for n, i, k2 in big:
        print(f"  {i+1:>2} -> {k2+1:<2}  all={recs[n]['count_all']} "
              f"cond={recs[n]['conductance']:.2f}  {recs[n]['content'][:56]}")

    # --- the spec's exact scenario -------------------------------------
    W = defaultdict(float)
    nbr = defaultdict(set)
    for e in g.cc_edges:
        if e["type"] in STRUCTURAL and e["source"] != e["target"]:
            x, y = sorted((e["source"], e["target"]))
            W[(x, y)] += e["weight"]
            nbr[x].add(y)
            nbr[y].add(x)

    print("\n=== the spec's scenario: five independent paths vs a single chain ===")
    print("Per (node, dependent) pair, R_eff on the symmetrized structural graph.")
    print("A single edge with w=0.5 gives R = 2.0 exactly; anything below 2.0 has")
    print("path multiplicity, and the deeper it falls the more independent support.\n")
    pairs = []
    for n, r in recs.items():
        for d in g.all_structural_dependents(n):
            pairs.append((n, d))
    # recompute resistances (cheap) for the histogram
    comp_of, comps = {}, []
    for x in g.nodes:
        if x in comp_of:
            continue
        st, cur = [x], []
        comp_of[x] = len(comps)
        while st:
            y = st.pop()
            cur.append(y)
            for z in nbr.get(y, ()):
                if z not in comp_of:
                    comp_of[z] = len(comps)
                    st.append(z)
        comps.append(sorted(cur))
    Rc = {}
    for ci, comp in enumerate(comps):
        if len(comp) < 2:
            continue
        loc = {x: i for i, x in enumerate(comp)}
        m = len(comp)
        A = np.zeros((m, m))
        for (x, y), w in W.items():
            if comp_of[x] == ci:
                A[loc[x], loc[y]] = A[loc[y], loc[x]] = w
        L = np.diag(A.sum(axis=1)) - A
        Lp = np.linalg.pinv(L, hermitian=True)
        dg = np.diag(Lp)
        Rc[ci] = (loc, dg[:, None] + dg[None, :] - 2 * Lp)

    def reff(u, v):
        if comp_of[u] != comp_of[v]:
            return float("inf")
        loc, R = Rc[comp_of[u]]
        return float(R[loc[u], loc[v]])

    rs = [(reff(n, d), n, d) for n, d in pairs]
    rs = [(r, n, d) for r, n, d in rs if np.isfinite(r)]
    rs.sort()
    bins = {"R<0.6 (deep support)": 0, "0.6<=R<1.2": 0, "1.2<=R<2.0": 0, "R==2.0 (single edge)": 0}
    for r, _, _ in rs:
        if r < 0.6:
            bins["R<0.6 (deep support)"] += 1
        elif r < 1.2:
            bins["0.6<=R<1.2"] += 1
        elif r < 2.0 - 1e-9:
            bins["1.2<=R<2.0"] += 1
        else:
            bins["R==2.0 (single edge)"] += 1
    for kk, v in bins.items():
        print(f"  {kk:<26} {v:>4}  ({v/len(rs):>5.1%})")

    print("\nthe five most deeply-supported (node -> dependent) pairs:")
    for r, n, d in rs[:5]:
        print(f"  R={r:.4f}   {g.snippet(n, 46)}")
        print(f"            -> {g.snippet(d, 60)}")

    excl_only = []
    for n, r in recs.items():
        cnt, excl = g.blast_radius(n)
        for d in excl:
            excl_only.append(reff(n, d))
    print(
        f"\nof the {len(excl_only)} pairs blast_radius actually counts, "
        f"{sum(1 for r in excl_only if r >= 2.0-1e-9)} sit at R=2.0 "
        f"({sum(1 for r in excl_only if r>=2.0-1e-9)/len(excl_only):.1%})"
    )
    print("So the discriminating resistance lives entirely OUTSIDE the set")
    print("blast_radius looks at.")

    (OUT / "h2_control.json").write_text(
        json.dumps(
            dict(
                k=k,
                jaccard_blast_vs_countall=j(warn_now, t_all),
                jaccard_blast_vs_cond=j(warn_now, t_cond),
                jaccard_countall_vs_cond=j(t_all, t_cond),
                tau_excl_all=tau("count_exclusive", "count_all"),
                tau_excl_cond=tau("count_exclusive", "conductance"),
                tau_all_cond=tau("count_all", "conductance"),
                resistance_bins=bins,
                n_big_moves=len(big),
                exclusive_pairs_at_R2=sum(1 for r in excl_only if r >= 2.0 - 1e-9),
                exclusive_pairs=len(excl_only),
            ),
            indent=1,
        )
    )
    print(f"\nwrote {OUT/'h2_control.json'}")


if __name__ == "__main__":
    main()
