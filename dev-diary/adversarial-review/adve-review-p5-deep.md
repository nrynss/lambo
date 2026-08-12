# Deep Adversarial Review — P5 Recall (`phase/p5-recall` @ 5dcf7ad)

**Reviewer:** AdversarialReviewP5 (independent deep pass)
**Mandate:** attempt to BREAK the prior ACCEPT claim on HEAD `5dcf7ad`; re-attack the 8 GPT5.6sol remediations (P1-1..P2-8) for regressions/over-/under-corrections; probe beyond the 8 (score lag, assertion-gated code, overflow, test honesty). READ-ONLY.

---

## Verdict: **ACCEPT** (no P1/P2 blockers)

The 8 remediations hold under independent adversarial re-inspection. The only genuine defects found are one **non-discriminating regression test** (P3) and two **latent / doc-vs-code** inconsistencies (P4). No data-corruption, correctness, concurrency, budget, or cache-consistency defect is reachable in a conforming graph. Product code for P1-1..P2-8 is otherwise sound.

---

## Findings (severity-ordered)

### F1 — [P3] P1-4 regression test `budget_charges_separators_and_enforces_ranked_prefix` does not discriminate (test honesty)
- **Location:** `src/recall/assemble.rs:912-968` (budget fixed at `34`, line 942)
- **FACT (computed):** with `token_fn = byte_len` and default weights (w_daemon=w_query=0.5), each rendered block `"concept N [Entity] (score 0.N0)"` is **31 bytes** (`"concept 1 [Entity] (score 0.50)"` = 31). Budget = 34. New code: block1 provisional 31 ≤ 34 → accept; block1+sep(2)+block2 = 64 > 34 → break → context = `[block1]`.
- **What pre-fix code would do:** separator-free cumulative charging: block1=31 ≤ 34 → accept; block1+block2 = 62 > 34 → reject → context = `[block1]`. **Identical output.** The discriminating window is budget ∈ {62,63} (old accepts 2 blocks at 62, new rejects at 64); the test never enters it. It also never constructs the ranked-prefix case (a *short* lower-ranked block that fits after a *skipped* higher-ranked one).
- **Why it matters:** the R3 closure claims this test guards the P1-4 separator-charging + ranked-prefix fix. It would **pass under the original broken implementation**, so it does not actually pin the remediation. (The product code is *correct* — I verified the exact-ranked-prefix and in-context separator charging at assemble.rs:214-227 — so this is a test-honesty gap, not a product bug.)
- **Direction:** use a budget in {62,63} to discriminate separator charging, and add a case with a small block ranked below a large one so that `context` becoming `[big]` (vs `[big, small]`) proves the stop-at-first-nonfit rule.
- **Confidence:** 0.85

### F2 — [P4] `blast_radii` (batch, used by the recall path) diverges from `blast_radius` (single) and all durable stores on structural self-loops
- **Location:** `src/recall/format.rs:119-129` vs `:109-115`
- **FACT:** `blast_radius` filters `*dst != node` (line 113); `blast_radii` (used by `assemble` at assemble.rs:152,173-174) has **no `dst != node` exclusion**. For a node N whose *only* inbound structural edge is a self-loop, `inbound_sources` yields `srcs(N) == {N}` with `srcs.len()==1`, so `blast_radii` counts N as its own dependent (1); `blast_radius` reports 0. The durable stores all exclude self (`c.id <> $node`, e.g. src/store/sqlite.rs:80) — so the RAM recall render would disagree with the store contract by +1.
- **Why it matters (reachability — weak):** `upsert_edge` stores what it is given (graph.rs:493-497) and a test proves a Dependency self-loop is accepted by upsert (graph.rs:2038), but cycle rejection lives in `record_action`'s BFS (graph.rs:490-492) and `assert_invariants` on every load (graph.rs:243). In a conforming graph the case is effectively unreachable — hence P4, not P2.
- **Test gap:** the two P2-7 tests (`blast_radius_matches_batched…`:537, `blast_radii_one_pass_agrees…`:586) only use acyclic graphs, so neither asserts the two functions agree for a self-loop.
- **Direction:** add the `dst != srcs[0]` guard to `blast_radii` (or drop the comparison claim) and add a self-loop case asserting consistency with `blast_radius`.
- **Confidence:** 0.6 (divergence is FACT; reachability is the weak link)

### F3 — [P4] "final score is finite for every input" doc claim not enforced for the `d`/`r` inputs
- **Location:** `src/recall/assemble.rs:104-118` and module doc lines 24-30
- **FACT/INFERENCE:** `sane_weight` sanitizes the *weights* only; `d` (ScoreTable) and `r` (phase-1 relevance) are multiplied unguarded. Daemon scores are finite by construction, but `r` can carry a store-provided vector similarity (candidates.rs:111) — a non-finite value would flow into `final_score` and rank first via `total_cmp`, contradicting the doc's "finite for every input" and the sanitization intent.
- **Reachability:** contrived (needs a misbehaving vector store). P4.
- **Direction:** guard `d`/`r` with the same finite/non-negative check used for weights, or scope the doc claim.
- **Confidence:** 0.4 (doc-vs-code mismatch is FACT; exploitability low)

---

## Re-attack of the 8 remediations — disposition

**P1-1 (single graph guard). HOLDS.** One `graph.read()` spans cache-get/compute/assemble (daemon/mod.rs:355-417), so key epoch == pipeline epoch == assembly graph. No TOCTOU: the guard is never released mid-pipeline, and no `.await` sits under any lock (gather is pre-lock). **Lock order:** recall acquires `graph.read() → scores.read() → index.read() → hot.write()`. I checked the loop's rescore path (mod.rs:727-731) acquires graph.read() then releases *before* `scores.write()` — same graph→scores direction, no inversion, no deadlock. The P2-8 session pre-check (mod.rs:330) reads a value (`session_id`) that is immutable per graph, so the pre-lock release there is not a TOCTOU.

**P1-2 (`cache only when embedding.is_none()`). AIRTIGHT.** `gather` returns an empty vector leg whenever `embedding` is `None` (candidates.rs:104-110), so cached pipelines can never contain vector candidates; `embedding.is_some()` forces `can_cache=false` (mod.rs:365), so a vector call never reads or writes the cache. No vector path can hit the cache and the keyword path can never serve vector-contaminated data. Test `recall_never_caches_vector_dependent_results` discriminates (asserts cache stays 0 for Some, 1 for None).

**P1-3 (keyword over-fetch + top_k at assembly). HOLDS.** `candidates` returns the unreduced union (candidates.rs:120-159); keyword is bounded by `limit.saturating_mul(KEYWORD_OVERFETCH)`; `expand` seeds from it; `assemble` decides top_k on final score (assemble.rs:148-207). No unbounded growth beyond the graph's own reachable BFS fan-out (bounded seed count). The golden was regenerated to the corrected semantics. P1-3 test `recent_leg_does_not_evict_keyword_matches` genuinely discriminates phase-1 survival.

**P1-4 (ranked prefix + separator charging + checked_add). PRODUCT CORRECT; TEST VACUOUS.** The loop measures `token_fn` over the *actual* joined string incl. `\n\n` separators (assemble.rs:214-227); `render_context` = `blocks.join("\n\n")` equals the last accepted provisional exactly, so `tokens ≤ max_tokens` is exact (no off-by-one; `== max_tokens` fits). `checked_add(1)` only guards the synthetic `+1` (dead in practice, harmless). See **F1** for the test-honesty gap.

**P2-5 (valid-member top_k). HOLDS.** `emitted` counts only graph-present non-forced hits; graph-missing members `continue` without a slot (assemble.rs:159-161, 203-205). Edge cases verified: **all-missing** → empty result (no panic/slot leak); **ties** → total order (`total_cmp` desc then id asc) so deterministic, every valid member in tie counts once. Test `stale_graph_missing_member_does_not_consume_top_k_slot` discriminates (would fail under old slot-counting with `[]` vs `[c1]`).

**P2-6 (candidates_without_keyword). HOLDS.** Recent + vector legs preserved, keyword leg simply absent (candidates.rs:166-190); entry routes index-absent to it (mod.rs:376) and appends the missing-index warning (mod.rs:410-415). Test `recall_without_index_keeps_recent_leg` discriminates.

**P2-7 (one-pass blast radii). HOLDS for conforming graphs.** `inbound_sources` + `blast_radii` is O(V+E); parallel/self edges are deduped; the two radial tests compare batch vs per-node over acyclic graphs and genuinely discriminate normal graphs. See **F2** for the self-loop divergence (unreachable via the write gate).

**P2-8 (session validation). HOLDS.** `recall` is the sole entry (no production caller outside tests; grep of `.recall(` across src yields none beyond daemon/tests), and the check is *inside* `recall`, so every path through it is gated. Mismatch → warning + empty, uses graph session for the vector namespace. `recall_rejects_mismatched_session` discriminates.

---

## Beyond-the-8 probes
- **Callers of rescore / scores-lag guard (P5-3):** inserting is skipped while `scores.epoch != epoch` (mod.rs:388), key preserved; cache-hit assembly still uses the live `scores` clone, so no inconsistent mix; test `recall_rescore_lag_guard…` discriminates the skip branch (R2-1). Verified.
- **Assertion-gated code:** `RecallCache::with_capacity(0)` asserts (cache.rs:98) — a call-site contract, unreachable via `Daemon::recall` (uses `RecallCache::new()`). No production panic path exercised by recall.
- **Overflow:** token arithmetic is String-length bounded; `checked_add` covers the theoretical overflow; no off-by-one vs `max_tokens`.
- **Graph/index read-pair (P5-4):** `gc::sync_index` inside the graph-write scope keeps the (graph, index) read pair atomic; recall's index read is taken under the single graph guard. Consistent.

---

## Confidence
Overall: 0.62 — the 8 remediations are well-constructed and I found no reachable P1/P2 defect, but the P1-4 regression test is demonstrably vacuous (the strongest concrete blemish) and the blast-radius/self-loop/store inconsistency is untested.


---

## Closure (integrator, 2026-08-12)

All three findings remediated and verified:

- **F1 (P3, test honesty)** — FIXED. The P1-4 budget test now uses the
  discriminating budget 63 (block1+block2 = 62 bytes fit the old separator-free
  code; 62 + `

` = 64 breaks the fixed code -> 1 block), and a new
  variable-size `ranked_prefix_stops_at_first_nonfit_block` test proves a short
  lower-ranked block never follows a skipped non-fitting top block. Both would
  fail under the pre-fix implementation.
- **F2 (P4)** — FIXED. `blast_radii` now applies the same `dst != node`
  self-exclusion as `blast_radius` and the durable stores; a self-loop no longer
  inflates a node's dependent count by 1. Consistency test added (batched vs
  single on a structural self-loop).
- **F3 (P4)** — FIXED. `final_score` sanitizes the daemon/relevance inputs with
  `sane_weight` (identity for all finite scores -> no golden change), honoring
  the "finite for every input" doc contract; `non_finite_daemon_or_relevance
  _inputs_sanitize_to_zero` pins it.

Gates after remediation: fmt clean; clippy `--all-targets -D warnings` clean;
default suite 426/0 (goldens byte-exact); sqlite-minimal clean. Committed as
`e056cd3` on `phase/p5-recall`.
