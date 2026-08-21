"""G3 step 2 — Hypothesis 1: phase-2 expansion as diffusion vs fixed-depth BFS.

What is actually being compared
-------------------------------
`assemble.rs` scores every member `d*w_daemon + r*w_query` where `r` is the
PHASE-1 relevance and is exactly 0.0 for any node that entered via expansion.
So fixed-depth expansion contributes no ordinal signal at all: it is a **binary
membership gate**. The honest comparison is therefore set-vs-set at equal
budget — take the fixed-depth expansion's non-seed members E_fixed, take the
top-|E_fixed| non-seed nodes by diffusion score, and measure the disagreement.
Ranking is reported as well (Kendall tau on the union), but with the caveat that
fixed-depth's only ordinal is the BFS level.

Variants
--------
* `fixed_directed`   — expand.rs verbatim: OUT-edges, 5 traversable types,
                       priority order, depth 2. The product's truth.
* `fixed_undirected` — same BFS on the symmetrized graph. A CONTROL: it
                       separates "diffusion is better" from "diffusion is merely
                       undirected", which would otherwise be confounded.
* `heat_t{T}`        — heat kernel exp(-t L_sym) on the symmetrized traversable
                       graph, seeded with the phase-1 score vector.
* `ppr_dir_a{A}`     — personalized PageRank on the DIRECTED traversable graph
                       (exact linear solve, no power iteration, no seeds needed
                       in the RNG sense — fully deterministic).
* `ppr_und_a{A}`     — personalized PageRank on the symmetrized graph.
* `*_typed`          — the same with type weights instead of unweighted.

Edge weighting argument
-----------------------
The snapshot carries weight 0.5 for ALL FOUR concept-concept types and
`reinforcements == 1` everywhere (see census): the stored weight and
reinforcement fields carry ZERO discriminating information on this graph, so a
data-driven weighting is unavailable. The only weighting signal that exists is
the edge TYPE, so the type-weighted variant borrows expand.rs's own priority
tiers: Dependency = Causal = 1.0 > Hierarchical = 0.7 > CoOccurrence = 0.4
(= Semantic, of which the snapshot has none).

`Derives` (Interaction->Concept) and `Temporal` (Interaction->Interaction) are
excluded from the primary diffusion for the same reason expand.rs excludes
them: they are the bipartite provenance backbone, not semantic relations, and
letting them diffuse would make every concept derived from one interaction a
neighbour of every other. Step 3 measures what INCLUDING them would do, because
the question "do the islands join through record_action provenance" is worth a
number.

Determinism: every solver here is a direct dense factorization
(`numpy.linalg.solve` / `scipy.linalg.expm`). No iterative solver, no RNG, no
seed to pin. Re-running reproduces every digit.
"""

import json
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np
from scipy.linalg import expm

from lambo_graph import DEFAULT_TRAVERSAL_DEPTH, TRAVERSAL_ORDER, LamboGraph

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
OUT.mkdir(exist_ok=True)

TYPE_WEIGHT = {
    "Dependency": 1.0,
    "Causal": 1.0,
    "Hierarchical": 0.7,
    "CoOccurrence": 0.4,
    "Semantic": 0.4,
}

HEAT_T = (0.5, 1.0, 2.0)
PPR_ALPHA = (0.15, 0.30, 0.50)


# ------------------------------------------------------------------ matrices


def adjacency(g, typed: bool, symmetric: bool):
    """Weighted adjacency over the five traversable Concept<->Concept types.

    `A[i, j] > 0` means an edge i -> j (source -> target), matching
    `out_neighbors_typed`. Self-loops dropped; parallel edges of different
    types sum.
    """
    n = len(g.nodes)
    idx = g.index_of
    A = np.zeros((n, n))
    for e in g.cc_edges:
        if e["type"] not in TRAVERSAL_ORDER:
            continue
        s, t = e["source"], e["target"]
        if s == t:
            continue
        w = TYPE_WEIGHT[e["type"]] if typed else 1.0
        A[idx[s], idx[t]] += w
    if symmetric:
        A = np.maximum(A, A.T)  # a relation is a relation in either direction
    return A


def laplacian(A):
    """Combinatorial Laplacian L = D - A of a symmetric A."""
    return np.diag(A.sum(axis=1)) - A


def heat_kernel(A, t):
    """exp(-t L). Dense matrix exponential; deterministic."""
    return expm(-t * laplacian(A))


def ppr_matrix(A, alpha):
    """Exact personalized PageRank operator.

    Solves (I - (1-alpha) P^T) X = alpha I for X, so `X @ s` is the PPR vector
    of seed distribution `s`. `P` is the row-stochastic random walk; rows with
    zero out-degree stay zero (substochastic), so probability mass leaks out of
    dangling nodes rather than teleporting uniformly. That choice is
    deliberate: uniform teleport from dangling nodes would hand every isolated
    concept in this island-heavy graph a floor of mass, manufacturing
    connectivity the graph does not have. Rankings are computed on the raw
    (unnormalized) vector, which is order-equivalent to normalizing.
    """
    n = A.shape[0]
    deg = A.sum(axis=1)
    P = np.zeros_like(A)
    nz = deg > 0
    P[nz] = A[nz] / deg[nz, None]
    return np.linalg.solve(np.eye(n) - (1.0 - alpha) * P.T, alpha * np.eye(n))


# ------------------------------------------------------------------ queries


def ledger_queries(path=HERE / "calls.jsonl"):
    """Every real lambo_recall call in the dogfood ledger, in ledger order."""
    qs = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        d = json.loads(line)
        if d.get("tool") != "lambo_recall":
            continue
        qs.append(
            dict(
                ts=d["ts"],
                agent=d.get("agent_id"),
                query=d["query"],
                top_k=d.get("top_k") or 10,
                depth=DEFAULT_TRAVERSAL_DEPTH,
                recorded_hits=[h["node_id"] for h in d.get("hits", [])],
            )
        )
    return qs


# ------------------------------------------------------------------ compare


def kendall_tau(order_a, order_b):
    """Kendall tau-b over the shared members of two ranked id lists."""
    shared = [x for x in order_a if x in set(order_b)]
    if len(shared) < 2:
        return None
    ra = {x: i for i, x in enumerate(order_a)}
    rb = {x: i for i, x in enumerate(order_b)}
    conc = disc = 0
    for i in range(len(shared)):
        for j in range(i + 1, len(shared)):
            x, y = shared[i], shared[j]
            s = (ra[x] - ra[y]) * (rb[x] - rb[y])
            if s > 0:
                conc += 1
            elif s < 0:
                disc += 1
    tot = conc + disc
    return (conc - disc) / tot if tot else None


def main():
    g = LamboGraph()
    n = len(g.nodes)
    idx = g.index_of
    qs = ledger_queries()
    print(f"=== G3 H1 — {len(qs)} real recall queries from the dogfood ledger ===")
    print(f"graph: {n} concepts, {len(g.cc_edges)} concept-concept edges\n")

    mats = {}
    for typed in (False, True):
        tag = "typed" if typed else "plain"
        A_dir = adjacency(g, typed, symmetric=False)
        A_und = adjacency(g, typed, symmetric=True)
        for t in HEAT_T:
            mats[f"heat_t{t}_{tag}"] = ("und", heat_kernel(A_und, t))
        for a in PPR_ALPHA:
            mats[f"ppr_dir_a{a}_{tag}"] = ("dir", ppr_matrix(A_dir, a))
            mats[f"ppr_und_a{a}_{tag}"] = ("und", ppr_matrix(A_und, a))
        print(f"built {tag} operators: heat {HEAT_T}, ppr {PPR_ALPHA}")
    print()

    # undirected BFS control
    und_adj = defaultdict(set)
    for e in g.cc_edges:
        if e["type"] in TRAVERSAL_ORDER and e["source"] != e["target"]:
            und_adj[e["source"]].add(e["target"])
            und_adj[e["target"]].add(e["source"])

    def bfs_und(seeds, depth):
        seen = set(seeds)
        frontier = list(seeds)
        out = [(s, 0) for s in seeds]
        for lvl in range(1, depth + 1):
            nxt = []
            for s in frontier:
                for t in sorted(und_adj.get(s, ())):
                    if t not in seen:
                        seen.add(t)
                        nxt.append(t)
                        out.append((t, lvl))
            if not nxt:
                break
            frontier = nxt
        return out

    rows = []
    for q in qs:
        p1 = g.phase1(q["query"], q["top_k"])
        seeds = [cid for cid, _, _ in p1]
        seed_scores = {cid: s for cid, s, _ in p1}
        if not seeds:
            rows.append(dict(query=q["query"], agent=q["agent"], empty_phase1=True))
            continue

        fixed = g.expand(seeds, q["depth"])
        e_fixed = [c for c, lvl in fixed if lvl > 0]
        fixed_levels = {c: lvl for c, lvl in fixed if lvl > 0}
        e_fixed_und = [c for c, lvl in bfs_und(seeds, q["depth"]) if lvl > 0]

        # seed vector: the phase-1 relevance the product already computed.
        s = np.zeros(n)
        for cid, sc in seed_scores.items():
            s[idx[cid]] = sc
        if s.sum() > 0:
            s = s / s.sum()

        row = dict(
            query=q["query"],
            agent=q["agent"],
            ts=q["ts"],
            top_k=q["top_k"],
            n_seeds=len(seeds),
            seeds=seeds,
            n_fixed=len(e_fixed),
            fixed=e_fixed,
            fixed_levels={c: fixed_levels[c] for c in e_fixed},
            n_fixed_und=len(e_fixed_und),
            fixed_und=e_fixed_und,
            variants={},
        )

        seedset = set(seeds)
        for name, (_kind, M) in mats.items():
            v = M @ s
            order = sorted(
                (x for x in g.nodes if x not in seedset),
                key=lambda x: (-v[idx[x]], x),
            )
            order = [x for x in order if v[idx[x]] > 1e-12]
            budget = len(e_fixed)
            top = order[:budget]
            sym = set(top) ^ set(e_fixed)
            row["variants"][name] = dict(
                n_reachable=len(order),
                top=top,
                jaccard=(
                    len(set(top) & set(e_fixed)) / len(set(top) | set(e_fixed))
                    if (top or e_fixed)
                    else 1.0
                ),
                sym_diff=len(sym),
                added=[x for x in top if x not in set(e_fixed)],
                dropped=[x for x in e_fixed if x not in set(top)],
                # Kendall tau of diffusion order against the BFS-level order
                # (ties inside a level are the BFS discovery order).
                tau_vs_level=kendall_tau(
                    order[: max(budget, 1)],
                    sorted(e_fixed, key=lambda c: (fixed_levels[c], e_fixed.index(c))),
                ),
            )
        rows.append(row)

    # ------------------------------------------------------ headline table
    live = [r for r in rows if not r.get("empty_phase1")]
    print(f"queries with a non-empty phase 1: {len(live)}/{len(qs)}")
    exp0 = [r for r in live if r["n_fixed"] == 0]
    print(
        f"queries where fixed-depth expansion added NOTHING: {len(exp0)}/{len(live)}"
        f"  ({len(exp0)/len(live):.0%})"
    )
    exp0u = [r for r in live if r["n_fixed_und"] == 0]
    print(f"  ... same on the UNDIRECTED graph: {len(exp0u)}/{len(live)}")
    print()

    print("fixed-depth expansion size per query (directed / undirected):")
    for r in live:
        print(
            f"  {r['n_seeds']:>2} seeds -> {r['n_fixed']:>3} dir / "
            f"{r['n_fixed_und']:>3} und   {r['query'][:58]}"
        )
    print()

    print("=== disagreement vs fixed_directed, at equal budget ===")
    print(f"{'variant':<26} {'mean J':>7} {'disagree':>9} {'ties-only':>10} {'mean tau':>9}")
    summary = {}
    for name in mats:
        js, dis, tie, taus = [], 0, 0, []
        for r in live:
            v = r["variants"][name]
            js.append(v["jaccard"])
            if v["sym_diff"] > 0:
                dis += 1
            elif r["n_fixed"] > 0 and v["tau_vs_level"] is not None and v["tau_vs_level"] < 1:
                tie += 1
            if v["tau_vs_level"] is not None:
                taus.append(v["tau_vs_level"])
        summary[name] = dict(
            mean_jaccard=float(np.mean(js)),
            n_set_disagreements=dis,
            n_queries=len(live),
            frac_set_disagreements=dis / len(live),
            n_rank_only_disagreements=tie,
            mean_tau=float(np.mean(taus)) if taus else None,
        )
        print(
            f"{name:<26} {np.mean(js):>7.3f} {dis:>4}/{len(live):<4} {tie:>10} "
            f"{(np.mean(taus) if taus else float('nan')):>9.3f}"
        )
    print()

    # undirected BFS control as a "variant"
    ju, du = [], 0
    for r in live:
        a, b = set(r["fixed"]), set(r["fixed_und"])
        ju.append(len(a & b) / len(a | b) if (a or b) else 1.0)
        if a ^ b:
            du += 1
    summary["CONTROL_fixed_undirected"] = dict(
        mean_jaccard=float(np.mean(ju)),
        n_set_disagreements=du,
        n_queries=len(live),
        frac_set_disagreements=du / len(live),
    )
    print(
        f"CONTROL fixed_undirected vs fixed_directed: mean J {np.mean(ju):.3f}, "
        f"set disagreements {du}/{len(live)}"
    )

    (OUT / "h1_rows.json").write_text(json.dumps(rows, indent=1))
    (OUT / "h1_summary.json").write_text(json.dumps(summary, indent=1, sort_keys=True))
    print(f"\nwrote {OUT/'h1_rows.json'} and {OUT/'h1_summary.json'}")


if __name__ == "__main__":
    main()
