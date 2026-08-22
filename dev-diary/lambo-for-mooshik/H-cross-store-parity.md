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
turns that from citation into measurement on our schema. Postgres carries hnsw **from
init** (decided 2026-08-19, see B2), so its lane is envelope-based from day one — plus a
**forced-exact lane** (`SET LOCAL enable_indexscan = off`) that detects adapter skew
exactly: approximation must come from the index, never from the dialect's SQL. The report
records per run whether an index was present.

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

- [x] H1 harness runs the full grid against SQLite + memory-oracle anywhere, with exact
      agreement asserted (this much runs in normal CI).
      `store::sqlite::tests::h1_cross_store_parity::h1_sqlite_and_memory_oracle_agree_exactly`
      (`src/store/sqlite.rs`) seeds both committed fixture graphs plus a stamped contract and
      `synthetic_unit_vector` — the same probe × limit grid as F's agreement matrix, reused
      rather than re-derived — into `SqliteStore` and into a second, independent `GraphStore`
      (`MemoryOracleStore`, an exact-cosine wrapper over `MemoryStore`; reimplemented rather
      than reusing `crate::memory`'s private `VectorSearchStore`, same precedent F's own
      `cosine_oracle` doc comment records for the same reason). Every probe × limit pair is
      asserted bit-for-bit equal (`assert!(pair.exact_match, ...)`) — not merely reported —
      per the attribution rule: two exact-scan adapters disagreeing is adapter skew, a bug.
      Not `#[ignore]`d, so it runs in the same CI row F's matrix does
      (`--features store-sqlite,fixtures`). A best-effort second leg (`run_real_embedder_leg`)
      additionally seeds from the committed `evidence/mooshik-f-sqlite-bge/f-bge.db` corpus
      (real BGE-M3 vectors) when present, skipping cleanly if not — the required evidence is
      the synthetic leg; the real-embedder leg is the "optionally" in "H1 — the harness".
      Every assertion was mutation-checked (perturb, confirm red, revert): reversing the
      oracle's sort direction, a 1% order-preserving score skew, and an off-by-one truncation
      each independently turned the corresponding measure (rank/candidate-set, score-only,
      candidate-set-size) red; a fourth check caught a real bug in the harness itself — the
      matrix-dimensions assertion was originally `>=` and stayed green when a probe/limit was
      dropped from the synthetic leg, because the optional real-embedder leg's extra pairs
      papered over the drop. Fixed to assert the synthetic leg's own count exactly.
      Report: `evidence/mooshik-h1-cross-store-parity/report.json` (schema documented on
      `ParityReport` in the harness module and in the evidence dir's README).
- [ ] H2 evidence committed: live Cockroach run, skew zero on the score scale, ANN
      divergence stated with numbers against the C-SPANN envelope
- [ ] F's Done-when box 5 flipped from `[~]` to done, citing H2's evidence
- [x] B3's parity box references this harness (note added to B doc at H1 landing)
- [x] The report format is stable enough that H2 and H3 evidence are directly comparable.
      `ParityReport` names adapters by free-text string, never by enum variant tied to a
      backend, so H2/H3 add rows (new `adapters` entries, new `pairs` entries) rather than new
      fields; `index_present` is a plain `bool` any backend's index-detection probe can set the
      same way; and every score in `pairs` is already on the shared `1 − d²/2 ≡ cosine` scale
      before it reaches the report, because that conversion happens inside each adapter's own
      `vector_candidates_checked` rather than in the harness.
