"""G3 step 5 — the three product-design questions, measured.

1. incremental maintenance — what does recomputing the operator cost, at this
   scale and at projected autobiography scale?
2. lock discipline — is the computation possible outside the graph lock?
3. determinism — is every number bit-reproducible?

These are the numbers §G3 demands "rather than an assumption" before any
adopt recommendation.
"""

import hashlib
import json
import time
from pathlib import Path

import numpy as np
from scipy.linalg import expm

from lambo_graph import LamboGraph
from step2_diffusion import adjacency, ppr_matrix

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"


def timeit(fn, reps=5):
    ts = []
    for _ in range(reps):
        t0 = time.perf_counter()
        r = fn()
        ts.append(time.perf_counter() - t0)
    return min(ts), r


def main():
    g = LamboGraph()
    n = len(g.nodes)
    print("=== G3 step 5 — cost and determinism ===\n")
    print(f"real graph: n = {n} concepts, {len(g.cc_edges)} concept-concept edges")
    print("machine: this host, single-threaded numpy/scipy dense linear algebra.\n")

    A_dir = adjacency(g, typed=False, symmetric=False)
    A_und = adjacency(g, typed=False, symmetric=True)

    rep = {"n": n, "measurements": {}}

    print("--- one-shot operator construction (the whole-graph precompute) ---")
    t, _ = timeit(lambda: ppr_matrix(A_dir, 0.3))
    rep["measurements"]["ppr_full_matrix_n386_ms"] = t * 1e3
    print(f"  PPR full n x n operator (dense solve)      {t*1e3:>8.1f} ms")
    t, _ = timeit(lambda: expm(-1.0 * (np.diag(A_und.sum(1)) - A_und)))
    rep["measurements"]["heat_expm_n386_ms"] = t * 1e3
    print(f"  heat kernel exp(-tL) (dense expm)          {t*1e3:>8.1f} ms")
    t, _ = timeit(lambda: np.linalg.pinv(np.diag(A_und.sum(1)) - A_und, hermitian=True))
    rep["measurements"]["pinv_n386_ms"] = t * 1e3
    print(f"  Laplacian pseudoinverse (SVD)              {t*1e3:>8.1f} ms")
    print()

    print("--- per-query cost, which is what recall actually pays ---")
    P = ppr_matrix(A_dir, 0.3)
    s = np.zeros(n)
    s[:12] = 1 / 12
    t, _ = timeit(lambda: P @ s, reps=50)
    rep["measurements"]["ppr_apply_precomputed_us"] = t * 1e6
    print(f"  apply a PRECOMPUTED operator (matvec)      {t*1e6:>8.1f} us")

    # single-seed-vector solve, no precompute: the incremental-friendly path
    deg = A_dir.sum(axis=1)
    Pw = np.zeros_like(A_dir)
    nz = deg > 0
    Pw[nz] = A_dir[nz] / deg[nz, None]
    M = np.eye(n) - 0.7 * Pw.T
    t, _ = timeit(lambda: np.linalg.solve(M, 0.3 * s), reps=20)
    rep["measurements"]["ppr_single_solve_ms"] = t * 1e3
    print(f"  ONE seed-vector solve, no precompute       {t*1e3:>8.2f} ms")
    print("    ^ this is the number that matters: no precompute to invalidate,")
    print("      so incremental maintenance is not required at all at this scale.")
    print()

    print("--- scaling: synthetic graphs at the same mean degree (2.7) ---")
    print(f"{'n':>7} {'edges':>7} {'1 solve':>11} {'full op':>11} {'pinv':>11}")
    rng = np.random.default_rng(20260821)  # PINNED SEED (issue #2's lesson)
    scale = {}
    for nn in (386, 1000, 3000, 10000):
        m = int(nn * 2.7 / 2)
        src = rng.integers(0, nn, m)
        dst = rng.integers(0, nn, m)
        Aq = np.zeros((nn, nn))
        Aq[src, dst] = 1.0
        np.fill_diagonal(Aq, 0.0)
        dg = Aq.sum(axis=1)
        Pq = np.zeros_like(Aq)
        nzq = dg > 0
        Pq[nzq] = Aq[nzq] / dg[nzq, None]
        Mq = np.eye(nn) - 0.7 * Pq.T
        sq = np.zeros(nn)
        sq[:12] = 1 / 12
        t1, _ = timeit(lambda: np.linalg.solve(Mq, 0.3 * sq), reps=3)
        t2, _ = timeit(lambda: np.linalg.solve(Mq, 0.3 * np.eye(nn)), reps=1)
        Lq = np.diag(dg) - Aq
        t3, _ = timeit(lambda: np.linalg.pinv(Lq, hermitian=False), reps=1)
        scale[nn] = dict(one_solve_ms=t1 * 1e3, full_op_ms=t2 * 1e3, pinv_ms=t3 * 1e3)
        print(f"{nn:>7} {m:>7} {t1*1e3:>9.1f}ms {t2*1e3:>9.1f}ms {t3*1e3:>9.1f}ms")
    rep["scaling"] = scale
    print()
    print("  Dense pinv is the expensive one and it is O(n^3): fine to n~3k,")
    print("  a problem at autobiography scale. A single PPR solve stays cheap")
    print("  and a sparse iterative solve would be cheaper still — but an")
    print("  iterative solver reintroduces the determinism question that the")
    print("  dense path answers for free.")
    print()

    print("--- lock discipline ---")
    t, _ = timeit(lambda: LamboGraph(), reps=3)
    rep["measurements"]["snapshot_load_ms"] = t * 1e3
    print(f"  full graph read out of SQLite               {t*1e3:>8.1f} ms")
    print("  The operators need ONLY (node_id, edge_type) triples: no content,")
    print("  no embeddings. Both diffusion and resistance are pure functions of")
    print("  the edge set, so the edge list can be copied under the graph lock")
    print("  and every solve run outside it. Recall §6.4 is satisfiable — the")
    print("  lock is held for the copy, not the algebra.")
    print(f"  edge-list copy is {len(g.cc_edges)} triples "
          f"(~{len(g.cc_edges)*40/1024:.0f} KiB); the algebra above is "
          f"{rep['measurements']['ppr_single_solve_ms']:.1f} ms outside it.")
    print()

    print("--- determinism ---")
    hashes = {}
    for label in ("run1", "run2"):
        vals = []
        gg = LamboGraph()
        Ad = adjacency(gg, typed=False, symmetric=False)
        Au = adjacency(gg, typed=False, symmetric=True)
        vals.append(ppr_matrix(Ad, 0.3))
        vals.append(expm(-1.0 * (np.diag(Au.sum(1)) - Au)))
        vals.append(np.linalg.pinv(np.diag(Au.sum(1)) - Au, hermitian=True))
        h = hashlib.sha256()
        for v in vals:
            h.update(np.ascontiguousarray(v, dtype=">f8").tobytes())
        hashes[label] = h.hexdigest()
    rep["determinism"] = hashes
    print(f"  run 1 sha256 {hashes['run1']}")
    print(f"  run 2 sha256 {hashes['run2']}")
    print(f"  bit-identical: {hashes['run1'] == hashes['run2']}")
    print("  Every solver used is a direct dense factorization (LAPACK gesv /")
    print("  gesdd / Pade expm). No RNG, no iteration count, no tolerance, no")
    print("  seed to pin. The one RNG in this file is the synthetic scaling")
    print("  graph, seeded 20260821.")

    (OUT / "cost.json").write_text(json.dumps(rep, indent=1))
    print(f"\nwrote {OUT/'cost.json'}")


if __name__ == "__main__":
    main()
