# Adversarial Review: P5 — Recall (branch `phase/p5-recall`) — phase close

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — remediated, verification ACCEPT                ║
║  Closed: 2026-08-12 — see Close record below                     ║
║  Scope:  P5 recall tier (T5.1–T5.4 + entry): candidates,         ║
║          expansion, assembly, context format, cache,             ║
║          Daemon::recall entry                                     ║
║  Source: phase/p5-recall @ 204e055 (diff vs main: 14 files,      ║
║          3,459 insertions)                                        ║
║  Reviewer: three parallel lenses (spec-§8 conformance /          ║
║          concurrency+lock discipline / cross-phase contracts +   ║
║          test honesty); orchestrated by the integrator           ║
║  Verdict: all three ACCEPT; 2 P2 + 4 P3 findings,                ║
║          remediated in one round (106d057), re-review ACCEPT      ║
╚══════════════════════════════════════════════════════════════════╝
```

## Reviewers and verdicts

- **Lens A (spec-§8 conformance)** — ACCEPT: phase-1 union (BM25 index, N=3
  recent by created_at, capability-gated vector), phase-2 BFS priority, phase-3
  scoring formula + hot re-validation + max_tokens, epoch-keyed cache, context
  format all conform. §3.2 degradation + RAM-tier promise verified.
- **Lens B (concurrency + locks + cache)** — ACCEPT: lock order cycle-free
  (G < I < H), no awaits under locks, cache probe/insert linearization sound,
  `&mut` cache across await safe, no deadlock for hot-write-under-graph-read.
- **Lens C (cross-phase contracts + test honesty + docs)** — ACCEPT: T4.2
  revalidate honored (recall's own now, read-time seconds_ago, ALGO-2 writer),
  T2.7 reservations rendered, golden byte-exact through the real entry and
  wall-clock-free, exit criteria [x] all backed by real tests.

## Findings (2 P2, 4 P3) and dispositions

- **P5-1 (P2, Lens A)** — Cache hits froze time-sensitive output: conflict
  `seconds_ago`, reservation expiry and hot-entry liveness were only
  re-validated on the compute path; spec §9 requires conditions re-validated on
  each `recall()`. **Remediated:** the cache now stores the epoch-stable
  pipeline artifact (`RecallPipeline { phase1, expanded }`); assembly, hot
  re-validation, reservations and rendering run on EVERY call. Regression
  tests: `recall_cache_hit_rerenders_fresh_warning_lines` (age refreshes
  11s -> 16s across a cache hit; lapsed window drops the line),
  `recall_reservation_transition_invalidates_cache_and_renders`.
- **P5-2 (P2, Lens C)** — Reservations never bumped `Graph::epoch`, so a
  reservation transition was invisible to the epoch-keyed cache (and the
  cache.rs "any graph mutation bumps epoch" claim overreached). **Remediated:**
  `set_reservation`/`clear_reservation` bump the epoch directly (reservations
  are RAM-local, no Mutation kind); cache.rs doc reworded to scope the claim;
  regression test proves the miss + re-render.
- **P5-3 (P3, Lens A + Lens B, merged)** — A compute whose daemon scores lag
  the graph epoch (up to one rescore tick) was cached under the new epoch key,
  freezing transient lagged scores. **Remediated:** the entry skips the cache
  insert while `scores.epoch != graph.epoch` (the spec key is preserved; only
  the lag window misses; the next call after the rescore recomputes).
- **P5-4 (P3, Lens B)** — `run_cycle` published GC collections into the index
  non-atomically with the graph (benign: assemble filters graph-missing
  members). **Remediated:** `gc::sync_index` now runs inside the graph-write
  scope, keeping recall's (graph, index) read pair atomic; lock order stays
  graph -> index.
- **P5-5 (P3, Lens C)** — Handoff Log silent on the conflict-writer planting:
  the golden's "Agent A wrote to it 11 seconds ago" is a planted-payload format
  pin; live detection on the fixture names agent-b (09:35Z). **Remediated:**
  reconciliation line added to the Handoff Log; T8.4 must arrange the live
  demo graph (writer agent-a, 11s ago) alongside the 9th dependent.
- **P5-6 (P3, Lens C)** — Stale duplicate "Exit criteria" checklist (unchecked
  [ ] block left at the phase-opening section). **Remediated:** folded into the
  closed-out checklist.

## Close record

Remediation round `106d057` (entry restructure + graph.rs reservation epoch +
cache genericization + run_cycle atomicity) verified by re-review (independent
pass over the remediation delta): all six dispositions verified against
source; the entry golden test still byte-exact; new regression tests genuinely
discriminate; full gates green — fmt, clippy `--all-targets -D warnings`,
default 415/0, sqlite 447/0, sqlite-minimal 340/0, cockroach 335/0, minimal +
demo checks clean.

## Round-2 verification (R2)

Independent re-review of the remediation delta (106d057 + 3a41105): ACCEPT with
2 P3s, both remediated (1d92a5d):

- **R2-1** — the rescore-lag guard's skip branch had no direct test (every test
  caught scores up first; removing the guard would pass). Fixed: new
  `recall_rescore_lag_guard_skips_cache_insert_while_scores_lag` — mutate
  without rescoring → cache.len() stays 1 (skip) while output still renders;
  rescore catch-up → len 2.
- **R2-2** — duplicate recall doc paragraphs (the restructure left two
  overlapping blocks). Fixed: collapsed to one block carrying the lock-order +
  pipeline detail.

Final gates after R2: fmt clean; clippy `--all-targets -D warnings` clean;
default 416/0; sqlite 448/0; sqlite-minimal 340/0; cockroach 335/0;
`recall_` 7/7 (golden byte-exact through the entry on first call AND hit).

**Disposition: ACCEPT — `phase/p5-recall` is merge-ready (completely clean).**

— integrator, orchestration + round-2 verification + R2 closure, 2026-08-12


## Round-3 closure (GPT5.6sol adversarial review, `5968276`)

A follow-up adversarial review (`adve-review-p5-recall-GPT5.6sol.md`) withdrew
the "completely clean" ACCEPT above, raising **4 P1 + 4 P2** findings. All were
validated against source and remediated (`b9e35ef`) with regression tests.

### Validated findings and remediation

- **P1-1** — cache probe epoch != pipeline epoch: gather (async, pre-lock)
  could assemble against a later graph epoch than the key. **Fixed:** one graph
  read guard spans cache-get/compute/assemble, so key epoch == pipeline epoch
  == assembly graph.
- **P1-2** — cache key omitted vector-source state (embedding presence, store
  failures, write-behind). **Fixed:** results are cached only when
  `embedding.is_none()`; the vector leg bypasses the cache entirely (correct
  over hit-rate; the keyword+recent demo/golden path keeps caching). Test:
  `recall_never_caches_vector_dependent_results`.
- **P1-3** — `candidates` truncated phase-1 to `limit`, so the flat 0.5 recent
  leg could evict strong keyword matches before assembly. **Fixed:**
  `KEYWORD_OVERFETCH=4` keyword over-fetch via `search(query, limit*4)`, drop
  the `truncate`; top_k decided at assembly on final score. Test:
  `recent_leg_does_not_evict_keyword_matches`; golden regenerated (byte-exact,
  matches both assemble and entry paths).
- **P1-4** — token budget never charged inter-block separators; ranked-prefix
  was broken; arithmetic unchecked. **Fixed:** context is built as an exact
  ranked prefix measuring `token_fn` on the real joined string (separators
  charged); `checked_add` with `break` on overflow/non-fit. Tests:
  ranked-prefix + separator charging in assemble.
- **P2-5** — `pos >= top_k` slot-counted graph-missing (stale vector) members.
  **Fixed:** only valid graph-present members count toward top_k (graph-missing
  skip, not slot). Test: stale-slot case in assemble.
- **P2-6** — `None => Vec::new()` discarded gathered recent/vector legs when
  the index is absent. **Fixed:** `candidates_without_keyword` preserves the
  recent leg (lexical lookup only is lost). Tests: format + daemon entry
  (`recall_without_index_keeps_recent_leg`).
- **P2-7** — `blast_radius` was O(CxE) per canonical hit. **Fixed:** one-pass
  `inbound_sources` + batched `blast_radii`; per-node wrapper kept. Tests: two
  blast-radius tests in format.
- **P2-8** — caller `session` not validated against the graph's authoritative
  session. **Fixed:** mismatch -> warning + empty result, never mixed; gather
  uses the graph session. Test: `recall_rejects_mismatched_session`.

### Gates after R3 (all green)

fmt clean; clippy `--all-targets -D warnings` clean; default suite 424/0;
store-sqlite ok; sqlite-minimal ok; minimal check ok; cockroach ok; demo check
ok. `recall_` entry tests 7/7 (incl. the new P1-2/P2-6/P2-8). Live-Cockroach +
fixture-write tests remain `#[ignore]`d as before (not part of these rows).

**Disposition: ACCEPT (revised) — all 8 GPT5.6sol findings remediated with
regression tests; gates green. P5 is NOT merged to main; local main stays at
`72f2a45` (`origin/main`).**

— integrator, R3 closure (GPT5.6sol round), 2026-08-12
