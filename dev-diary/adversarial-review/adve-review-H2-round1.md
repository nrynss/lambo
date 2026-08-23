# Adversarial Review — Mooshik H2 (live Cockroach parity leg), round 1

- **Reviewer:** `H2Verify` (fresh independent reviewer; code read-only)
- **Date:** 2026-08-23
- **Scope:** commits `c7752ec` (test) + `623fe14` (+ `9a699f4` docs) on
  `lambo-for-mooshik` at HEAD `9a699f4`; evidence dir
  `evidence/mooshik-h2-cockroach-parity/` (`report.json`, `run-20260823-live.txt`,
  `README.md`); test module `store::cockroach::h2_cockroach_parity`
  (`src/store/cockroach.rs:5659-6544`)
- **Verdict:** **APPROVE** — 0 P0, 0 P1, 0 P2, 2 P3 (hardening notes;
  non-blocking, neither falsifies the committed evidence)

The implementer's claims survive falsification attempts on every axis:
the live run is real and reproduces green independently, every asserted
number appears in the emitted report and matches the prose, the assertions
compare measured values against named constants (no tautology found), the
index proof is a genuine in-test EXPLAIN gate, and no DSN material exists
in any committed artifact.

## Independent re-run (the load-bearing check)

Re-ran the live leg myself from a clean tree at HEAD, DSN sourced from
`.env` without echoing, committed artifacts untouched (no
`LAMBO_H2_EMIT_EVIDENCE`):

```
LAMBO_REQUIRE_LIVE=1 cargo test --features store-cockroach,store-sqlite,fixtures \
  --lib store::cockroach::h2_cockroach_parity -- --ignored --nocapture
```

Result: **ok, 22.92s**, with output identical to the committed log where it
must be: cluster fingerprint `CockroachDB CCL v26.2.5 (…go1.25.5)`, the same
`SHOW CREATE TABLE concepts` carrying
`VECTOR INDEX concepts_embedding_idx (embedding vector_l2_ops) WHERE embedding IS NOT NULL`,
a genuine planner EXPLAIN showing `• vector search / table:
concepts@concepts_embedding_idx (partial index)` with no full scan, quarantine
leg `cockroach 5, sqlite 5 → sqlite EMPTY after 9/9 NULLed, cockroach refused`,
and the summary line `120 pairs … min jaccard 1, max score skew
0.000005364418029785156, total displaced ranks 0; fully-agreeing ANN cells:
80/80` — the max skew **bit-identical** to both the committed report.json and
the run log (deterministic synthetic corpus, so exact reproduction is the
expected signature of an honest capture; an authored number would be unlikely
to reproduce to 17 significant digits through a different process run).
This was not authored prose.

## Claim-by-claim verification

1. **80/80 ANN agreement, min jaccard 1.0, displaced 0** — verified by
   recomputation from `report.json` (not from the README): 80 `AnnEnvelope`
   rows, `min(candidate_jaccard) == 1.0`, `sum(len(displacement)) == 0`, all
   `rank_prefix_match` ≥ effective limit. Asserted against bound
   `ANN_JACCARD_FLOOR = 0.98` with the jaccard⇔recall derivation documented
   (`jaccard = inter/(2k−inter)` at equal-size top-k ⇒ recall ≥ 0.99, the
   C-SPANN published envelope at beam 64). Non-vacuous: the assertion at
   `src/store/cockroach.rs:6265-6271` compares each measured pair value to
   the constant.
2. **Max skew 5.364418029785156e-6 vs bound 1e-4** — recomputed from
   report.json, matches README ("5.36e-6") and run log exactly. The
   "conversion bug would sit ≥ ~1e-2" claim is sound: `1 − d` vs `1 − d²/2`
   differs by ~d²/2 (≈0.5 at unit distance) and any scale factor error is
   larger still — three-plus orders above the bound
   (`SCORE_SKEW_EPSILON`, `src/store/cockroach.rs:5745`). The measured value
   is consistent with f32 accumulation noise as claimed.
3. **Attribution split 40 ExactMustMatch + 80 AnnEnvelope = 120** — verified
   from report.json and re-derived from code: grid is
   2 fixtures × 4 probes × 5 limits × C(3,2)=3 pairs. The five limits are
   corpus-derived per fixture (`[1, 3, 5, pool, pool+7]`,
   `src/store/cockroach.rs:6217`): rest-api pool=22 → {1,3,5,22,29},
   drift pool=9 → {1,3,5,9,16} — exactly what report.json records. The
   test hard-asserts the total (`assert_eq!(report.pairs.len(), 2*4*5*n_pairs)`,
   `:6482-6486`).
4. **`index_present=true` measured, not assumed** — confirmed as a *gate*:
   `assert_index_backed` runs plain `EXPLAIN {VECTOR_CANDIDATES_SQL}` (the
   production constant itself) against the live cluster before the cockroach
   adapter is registered, and panics unless the plan shows `vector search`
   on `concepts@concepts_embedding_idx` with no `FULL SCAN`
   (`:6035-6062`). A run that reaches report emission cannot have failed it.
   See P3-2 for the residual wiring nit.
5. **Quarantine leg settles F box 5 by measurement** — code path verified
   (`:6300-6409`): under contract A both adapters answer non-empty; SQLite
   restamp-quarantine asserted to NULL **all** concept vectors (9/9,
   `:6364-6372`) and answer EMPTY (`:6379-6384`); Cockroach's width-4
   question asserted to error via the `VECTOR(1024)` DDL gate
   (`:6394-6398`). Zero cross-space candidates delivered on both — the F
   doc flip (`F-sqlite-vectors.md`) quotes this accurately.
6. **Cluster hygiene + zero DSN leakage** — corpus is rebased into a fresh
   per-run session scope (`rebase_into_fresh_scope` with UUID suffix);
   only `init_schema()` (idempotent DDL) plus inserts into run-owned
   sessions; nothing pre-existing read, touched, or dropped. Leakage sweep:
   grep over **all git-tracked files** for DSN-shaped strings — every hit is
   a placeholder (`USER`/`<user>:<password>`/`HOST` templates in examples,
   docs, runbooks); the H2 evidence dir (README, report.json string fields,
   run log) is clean; `.env` is gitignored. No credential material
   committed.
7. **Evidence integrity** — report.json parses; its numbers match README
   prose and the run log line-for-line, including the subtle one: the README's
   "16 rows where `rank_prefix_match < limit` are truncation at pool size"
   checks out exactly (8 probe/limit combos × 2 pairs, prefix == pool size
   22 or 9 in every case). Run log carries real cluster fingerprints
   (server version string, full DDL, planner output). All 40 ExactMustMatch
   rows have `exact_match: true`; ANN rows' `exact_match` false only by
   float noise, as stated.

## Vacuity hunt (explicit)

- **No tautologies.** Every pass/fail branch compares a measured pair field
  to a named constant or between two independently computed adapter results:
  `exact_match = got_a == got_b` is a real bit-comparison of ids **and**
  f64 scores across `MemoryOracleStore` (direct f32 cosine oracle,
  `vector_candidates_checked` reimplemented with an unchecked-path panic
  guard) vs `SqliteStore`; the honesty check at `:6490-6496` re-asserts that
  no recorded ExactMustMatch row lacks its assertion.
- **Empty-result hole exists but is refuted for this evidence (P3-1).**
  `jaccard()` special-cases both-empty to 1.0 and there is no per-cell
  non-empty assertion, so a hypothetical regression emptying results on all
  three adapters would go green with a vacuously perfect report. It did not
  happen here: the committed report itself proves non-empty candidate sets
  (`rank_prefix_match` = 22/9 at the pool+7 limits requires ≥pool results
  from both sides of each pair, across all 4 probes × both fixtures), the
  quarantine leg printed live non-empty counts, and my re-run reproduced it.
  The hole is a harness robustness gap, not an evidence flaw.

## Findings

### H2-R1-1 (P3) — grid runner has no per-cell non-empty guard; both-empty would pass vacuously

- **Evidence:** `run_fixture_grid` (`src/store/cockroach.rs:6230-6281`)
  computes `candidate_jaccard`/`displacement`/`max_score_diff` without ever
  asserting `!got_a.is_empty()`; `jaccard()` returns 1.0 for two empty sets
  (`:5910-5911`); `PairResult` records no candidate counts, so the emitted
  report alone could not distinguish perfect recall from no recall.
- **Impact:** Future harness/corpus regressions that silently empty every
  result would produce a green, maximally-flattering report. Not exercised
  here — the committed data and live re-run prove full-size candidate sets
  (see vacuity hunt).
- **Fix:** assert `!got_a.is_empty() && !got_b.is_empty()` per cell (and/or
  record `len_a`/`len_b` in `PairResult`).

### H2-R1-2 (P3) — `AdapterRun.index_present` is a hardcoded literal, not wired from the measurement

- **Evidence:** `assert_index_backed(&cr).await` gates registration
  (`:6443`), but the recorded field is `index_present: true` written by hand
  into the `Adapter` literal (`:6447`) and copied verbatim into the report
  (`:6198`). The measurement and the recorded bit are connected only by
  ordering, not by data flow.
- **Impact:** Desync-by-refactor risk only (e.g. someone moves the EXPLAIN
  call after grid construction and the report keeps claiming `true`). The
  code comment already states the intent honestly; today's evidence is
  valid because the gate demonstrably ran (EXPLAIN text in the log).
- **Fix:** have `assert_index_backed` return `bool` (or a plan digest) and
  feed it into the `Adapter`.

## Non-findings checked and cleared

- **serve_proxy_multi_client "pre-existing failures" claim:** c7752ec's diff
  touches only `#[cfg(test)]` code inside `src/store/cockroach.rs` — no
  production code, no shared state with the J2 stdio-proxy suite (which
  binds no fixed port and uses per-test temp dirs). Prior independent review
  (`adve-review-mooshik-J3-redesign-round3.md`) already measured that suite
  passing in isolation and in-suite on this machine. Nothing in the H2 work
  can have introduced those failures.
- **Twin-module precedent:** the reimplementation (vs extracting H1's
  harness) follows the repo's recorded private-test-infra precedent and is
  argued in the module docs; H1's landed module is untouched (diff scope).
- **Report schema stability:** v1 shape identical to H1's; only rows added,
  as the README states.

## Verdict

**APPROVE.** The H2 Done-when closure (H boxes ticked, F box 5 flipped) rests
on real, reproducible, non-vacuous evidence. The two P3 findings are
hardening items for the harness, not defects in the claim or its capture;
neither blocks the closure.
