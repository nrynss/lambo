"""G3 step 3 — Hypothesis 2: blast radius as effective resistance, not a count.

What `blast_radius` actually is
------------------------------
Not "count of dependents". Per `src/store/sqlite.rs::blast_radius` (and the
MemoryStore / Cockroach twins), it is the count of OTHER concepts that have an
aged inbound {Dependency, Causal, Hierarchical} edge FROM `node` and NO aged
inbound structural edge from ANY OTHER concept source. That is: **exclusive
dependents** — the concepts that would be orphaned if `node` went away.

That definition already encodes a crude notion of sole support, which is what
makes H2's premise worth checking rather than assuming: the spec's picture of "a
dependent connected by five independent paths" is a dependent that, by
construction, has inbound structural edges from other sources — so
`blast_radius` does not count it at all.

Effective resistance
--------------------
R_eff(u,v) = L†_uu + L†_vv - 2·L†_uv on the symmetrized structural graph,
computed PER CONNECTED COMPONENT (the pseudoinverse of a disconnected Laplacian
yields finite but meaningless cross-component values; resistance between
components is infinite and is reported as such). Low resistance = many
independent paths = deeply load-bearing. Conductance 1/R is the natural
per-node aggregate.

Three rankings are compared:
  count_exclusive  — the product's `blast_radius` today
  count_all        — every structural dependent, exclusive or not
  conductance      — sum over structural dependents of 1/R_eff(node, dep)

Determinism: `numpy.linalg.pinv` (SVD-based, no RNG). Reproducible bit for bit.
"""

import json
from collections import defaultdict
from pathlib import Path

import numpy as np

from lambo_graph import STRUCTURAL, LamboGraph

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
OUT.mkdir(exist_ok=True)

MIN_BLAST_RADIUS = 5  # src/canon/stage3.rs — Stage 3 needs blast > 5
WARN_TOP = 9  # size of the set that clears that bar on this snapshot


def main():
    g = LamboGraph()
    print("=== G3 H2 — blast radius: count vs effective resistance ===\n")

    # ---- symmetrized structural graph ---------------------------------
    W = defaultdict(float)
    nbr = defaultdict(set)
    for e in g.cc_edges:
        if e["type"] not in STRUCTURAL or e["source"] == e["target"]:
            continue
        a, b = sorted((e["source"], e["target"]))
        W[(a, b)] += e["weight"]  # parallel edges of different types add
        nbr[a].add(b)
        nbr[b].add(a)
    print(f"structural edges (directed rows)   {sum(1 for e in g.cc_edges if e['type'] in STRUCTURAL)}")
    print(f"distinct undirected structural pairs {len(W)}")
    parallel = sum(1 for k, v in W.items() if v > 0.5)
    print(f"pairs carrying >1 structural type    {parallel}")

    # ---- components ---------------------------------------------------
    comp_of, comps = {}, []
    for x in g.nodes:
        if x in comp_of:
            continue
        stack, cur = [x], []
        comp_of[x] = len(comps)
        while stack:
            y = stack.pop()
            cur.append(y)
            for z in nbr.get(y, ()):
                if z not in comp_of:
                    comp_of[z] = len(comps)
                    stack.append(z)
        comps.append(sorted(cur))
    print(f"components {len(comps)}, largest {max(len(c) for c in comps)}")

    # ---- per-component Laplacian pseudoinverse ------------------------
    Rcache = {}
    for ci, comp in enumerate(comps):
        if len(comp) < 2:
            continue
        loc = {x: i for i, x in enumerate(comp)}
        m = len(comp)
        A = np.zeros((m, m))
        for (a, b), w in W.items():
            if comp_of[a] == ci:
                A[loc[a], loc[b]] = w
                A[loc[b], loc[a]] = w
        L = np.diag(A.sum(axis=1)) - A
        Lp = np.linalg.pinv(L, hermitian=True)
        d = np.diag(Lp)
        R = d[:, None] + d[None, :] - 2 * Lp
        Rcache[ci] = (loc, R)

    def r_eff(u, v):
        if comp_of[u] != comp_of[v]:
            return float("inf")
        loc, R = Rcache[comp_of[u]]
        return float(R[loc[u], loc[v]])

    # ---- rankings ------------------------------------------------------
    recs = []
    for x in g.nodes:
        cnt, excl = g.blast_radius(x)
        alld = g.all_structural_dependents(x)
        if not alld:
            continue
        cond = 0.0
        rs = []
        for d in alld:
            r = r_eff(x, d)
            rs.append(r)
            if np.isfinite(r) and r > 0:
                cond += 1.0 / r
        recs.append(
            dict(
                node=x,
                content=g.snippet(x, 150),
                count_exclusive=cnt,
                count_all=len(alld),
                conductance=cond,
                min_r=min(rs) if rs else None,
                max_r=max(rs) if rs else None,
                mean_r=float(np.mean(rs)) if rs else None,
                exclusive_r=[r_eff(x, d) for d in excl],
            )
        )
    print(f"\nconcepts with >=1 structural dependent: {len(recs)}")

    # ---- the analytic claim, checked ----------------------------------
    # An exclusive dependent has no other structural in-edge, so the only way
    # it can reach `node` by a second independent path is UNDIRECTED (through
    # its own out-edges). Does that ever happen here?
    all_excl_r = [r for rec in recs for r in rec["exclusive_r"]]
    single = sum(1 for r in all_excl_r if abs(r - 2.0) < 1e-9)  # 1/w, w=0.5
    print(f"\nexclusive-dependent resistances: {len(all_excl_r)} pairs")
    print(f"  R == 1/w == 2.0 exactly (single edge, no multiplicity): {single}")
    print(f"  R < 2.0 (some path multiplicity found):                 "
          f"{sum(1 for r in all_excl_r if r < 2.0 - 1e-9)}")
    if all_excl_r:
        print(f"  min {min(all_excl_r):.4f}  max {max(all_excl_r):.4f}")
    print(
        "  -> effective resistance is CONSTANT across the set blast_radius\n"
        "     counts, so it cannot reorder that set."
        if single == len(all_excl_r)
        else "  -> some multiplicity exists; resistance can reorder."
    )

    # ---- rank comparison ----------------------------------------------
    def rank(key, rev=True):
        return sorted(recs, key=lambda r: (-r[key] if rev else r[key], r["node"]))

    by_excl = rank("count_exclusive")
    by_all = rank("count_all")
    by_cond = rank("conductance")

    def tau(a, b):
        ra = {r["node"]: i for i, r in enumerate(a)}
        rb = {r["node"]: i for i, r in enumerate(b)}
        ns = list(ra)
        c = d = 0
        for i in range(len(ns)):
            for j in range(i + 1, len(ns)):
                x, y = ns[i], ns[j]
                s = (ra[x] - ra[y]) * (rb[x] - rb[y])
                if s > 0:
                    c += 1
                elif s < 0:
                    d += 1
        return (c - d) / (c + d) if (c + d) else None

    print("\nKendall tau between rankings over the 65-concept dependent set:")
    print(f"  count_exclusive vs conductance  {tau(by_excl, by_cond):+.3f}")
    print(f"  count_all       vs conductance  {tau(by_all, by_cond):+.3f}")
    print(f"  count_exclusive vs count_all    {tau(by_excl, by_all):+.3f}")

    # ---- the warning-worthy set ---------------------------------------
    warn_now = [r for r in recs if r["count_exclusive"] > MIN_BLAST_RADIUS]
    print(
        f"\nwarning-worthy TODAY (blast_radius > {MIN_BLAST_RADIUS}, the Stage-3 "
        f"bar): {len(warn_now)} concepts"
    )
    top_cond = by_cond[: max(len(warn_now), WARN_TOP)]
    a = {r["node"] for r in warn_now}
    b = {r["node"] for r in top_cond}
    print(f"resistance top-{len(top_cond)} by conductance vs that set:")
    print(f"  overlap {len(a & b)}/{len(a)}   Jaccard {len(a&b)/len(a|b):.3f}")

    print("\n--- count_exclusive ranking (the product's warning order) ---")
    print(f"{'blast':>5} {'all':>4} {'cond':>8}  content")
    for r in by_excl[:14]:
        flag = "*" if r["count_exclusive"] > MIN_BLAST_RADIUS else " "
        print(
            f"{r['count_exclusive']:>5}{flag}{r['count_all']:>4} "
            f"{r['conductance']:>8.3f}  {r['content'][:78]}"
        )

    print("\n--- conductance ranking (resistance's warning order) ---")
    print(f"{'cond':>8} {'blast':>5} {'all':>4}  content")
    for r in by_cond[:14]:
        flag = "*" if r["count_exclusive"] > MIN_BLAST_RADIUS else " "
        print(
            f"{r['conductance']:>8.3f} {r['count_exclusive']:>5}{flag}"
            f"{r['count_all']:>4}  {r['content'][:78]}"
        )

    print("\n--- disagreements: in one top set but not the other ---")
    dis = []
    for r in top_cond:
        if r["node"] not in a:
            print(
                f"  resistance PROMOTES (blast={r['count_exclusive']}, "
                f"all={r['count_all']}, cond={r['conductance']:.2f}):"
            )
            print(f"      {r['content']}")
            dis.append(dict(kind="promoted", **{k: r[k] for k in
                       ("content", "count_exclusive", "count_all", "conductance")}))
    for r in warn_now:
        if r["node"] not in b:
            print(
                f"  resistance DEMOTES (blast={r['count_exclusive']}, "
                f"all={r['count_all']}, cond={r['conductance']:.2f}):"
            )
            print(f"      {r['content']}")
            dis.append(dict(kind="demoted", **{k: r[k] for k in
                       ("content", "count_exclusive", "count_all", "conductance")}))
    if not dis:
        print("  (none — the two top sets are identical)")

    # ---- per-dependent discrimination: does resistance vary at all? ----
    all_r = [
        r_eff(rec["node"], d)
        for rec in recs
        for d in g.all_structural_dependents(rec["node"])
    ]
    fin = [r for r in all_r if np.isfinite(r)]
    uniq = sorted({round(r, 6) for r in fin})
    print(
        f"\nper-dependent resistance across all {len(fin)} (node, dependent) "
        f"pairs: {len(uniq)} distinct values"
    )
    print(f"  min {min(fin):.4f}  max {max(fin):.4f}  "
          f"pairs with R < 2.0 (multiplicity): {sum(1 for r in fin if r < 2.0-1e-9)}")
    print(f"  distinct values (first 12): {uniq[:12]}")

    (OUT / "h2.json").write_text(
        json.dumps(
            dict(
                records=recs,
                tau_excl_cond=tau(by_excl, by_cond),
                tau_all_cond=tau(by_all, by_cond),
                warn_now=[r["node"] for r in warn_now],
                top_cond=[r["node"] for r in top_cond],
                disagreements=dis,
                exclusive_r_all_single=(single == len(all_excl_r)),
                n_exclusive_pairs=len(all_excl_r),
                n_pairs_with_multiplicity=sum(1 for r in fin if r < 2.0 - 1e-9),
                n_pairs=len(fin),
                distinct_resistances=len(uniq),
            ),
            indent=1,
        )
    )
    print(f"\nwrote {OUT/'h2.json'}")


if __name__ == "__main__":
    main()
