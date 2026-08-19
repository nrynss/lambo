# H — Cross-store recall parity, measured live

**Goal:** one harness that measures vector-recall divergence between adapters on the same
seeded graph, run live against Cockroach now and against Postgres when B lands.

**Where this came from:** F's Done-when box 5 ("recall parity between SQLite and
Cockroach") closed at its cluster-free half — the agreement matrix proves SQLite equals an
exact-cosine oracle, and the score-scale test proves both adapters mean the same thing by
a score. What remains unmeasured is the live half: how far Cockroach's C-SPANN ANN
candidates diverge from exact, and whether any divergence is approximation or adapter
skew. B3 then lands a *third* distance pipeline (pgvector `<=>` cosine distance, score
`1 - d`, a different operator *and* a different conversion than Cockroach's `<->` L2 with
`1 - d²/2`) whose done-when already demands "ranking parity with Cockroach … verified
rather than assumed." Without a shared harness, that box gets verified three different
ad-hoc ways or not at all.

**Cannot run from every machine, by design.** H2/H3 need a live DSN. That is an
environment fact, not a task defect: the harness (H1) is buildable and testable anywhere;
the live legs run where credentials exist — the Linux box that ran the C-series capture,
the desktop, or CI after this branch merges (the `cockroach-live` job is gated off on
`lambo-for-mooshik` and comes back at merge).

---

## What "parity" means here, precisely

For a seeded graph and probe set, per adapter pair:

1. **Candidate-set agreement** — Jaccard of the top-k id sets, per probe × k.
2. **Rank agreement** — exact prefix match length, plus displacement per shared id.
3. **Score agreement** — after each adapter's documented conversion to the shared
   `1 − d²/2 ≡ cosine` scale (valid because unit-norm output is now an explicit
   `Embedder::embed` contract — F round-1 remediation).

Divergence is then attributed: exact-scan adapters (SQLite, memory oracle) must agree
**exactly** — any diff is adapter skew, a bug. ANN adapters (Cockroach) may diverge within
a stated envelope — C-SPANN's published figure is 0.99 recall@50 at beam 64; the harness
turns that from citation into measurement on our schema. pgvector's envelope depends on
the index B3 chooses (ivfflat/hnsw) and becomes part of B3's record.

---

## H1 — The harness

A binary or `#[ignore]`d test target that:

* seeds the two committed fixture graphs plus a stamped contract and the synthetic
  unit-vector set (reuse `synthetic_unit_vector` and the matrix's probe/limit grid — same
  shape, cross-adapter instead of adapter-vs-oracle),
* optionally seeds a real-embedder set (the BGE-M3 rig produces one; the committed
  `evidence/mooshik-f-sqlite-bge/` vectors are a starting corpus),
* runs the identical probe × limit grid through `vector_candidates_checked` on every
  adapter reachable from config, and
* emits a machine-readable report (JSON) of the three agreement measures, suitable for
  committing under `evidence/`.

Adapters take no new code: the harness is a caller. Where it lives (tests vs
`scripts/`-style driver) follows whatever `cockroach-live`'s existing `#[ignore]`d tests
already do — do not invent a second live-test convention.

**Depends on:** F (landed, `9c2da7e`).

---

## H2 — The Cockroach live leg

Run H1 against a live cluster (DSN via `LAMBO_COCKROACH_DSN`, as the existing live tests
do). Capture under `evidence/` with the run's cluster shape, index parameters, and the
attribution: skew must be zero on the score scale; candidate/rank divergence within the
ANN envelope, stated with numbers.

Also settles, with a measurement, the question F could only reason about: whether
Cockroach's NULL-only quarantine plus DDL width enforcement and SQLite's write-gate plus
restamp-quarantine produce the same *observable* recall behaviour on the same history of
writes (the round-2 review verified the reasoning; this verifies the behaviour).

**Depends on:** H1, plus a machine with a DSN — not this laptop. Runs on the Linux box,
the desktop, or CI post-merge.

---

## H3 — The Postgres leg

When B3 lands, the same harness, no changes, against pgvector. This *is* B3's parity
done-when box — B should reference H rather than re-specify it. The dangerous item B3
names (cosine-distance conversion `1 - d`, silently wrong ranking if fumbled) is exactly
what the score-agreement measure catches: a conversion error shows as systematic score
skew at zero candidate divergence.

**Depends on:** H1, B3.

---

## Done when

- [ ] H1 harness runs the full grid against SQLite + memory-oracle anywhere, with exact
      agreement asserted (this much runs in normal CI)
- [ ] H2 evidence committed: live Cockroach run, skew zero on the score scale, ANN
      divergence stated with numbers against the C-SPANN envelope
- [ ] F's Done-when box 5 flipped from `[~]` to done, citing H2's evidence
- [ ] B3's parity box references this harness (note added to B doc at H1 landing)
- [ ] The report format is stable enough that H2 and H3 evidence are directly comparable
