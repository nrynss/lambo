# P6 — Canonization

```yaml
id:       P6
branch:   phase/p6-canonization
requires: [P1, T4.5]     # Stage 1 consumes gc_survived; stages test fixture-first via MemoryStore
blocks:   P8 demo story
parallel: medium   # T6.1 ‖ T6.2 ‖ T6.3 (independent predicates); T6.4 integrates
runs-parallel-with: P2, P3, P5, P7
```

**Goal:** spec §10, retained whole. **This is the differentiator and it does not get cut.**
Nobody else ships memory that decides for itself what is load-bearing.

All three stage predicates are pure functions of (graph state, store queries, config,
clock) — build and test each against `MemoryStore` + `session-rest-api` (which plants a
lawful full-progression node, T1.4), swap to live structural queries when T3.6 lands.
Inject the clock; every threshold here is time-sensitive and untestable otherwise.

---

### T6.1 — Stage 1: Candidate
```yaml
requires:   T1.1, T4.1
fixture-ok: yes
owns:       src/canon/stage1.rs
status:     done
```
`gc_survived >= 3` AND composite score above the 90th percentile of non-Canonical peers —
evaluated only when ≥ `canonization_min_peer_count=20` non-Canonical concepts exist.
Percentile over daemon scores (T4.1's output).

**Done when:** peer-count gate, percentile edge (exactly-at-P90), and the fixture's
planted candidate all behave.

---

### T6.2 — Stage 2: Venerable
```yaml
requires:   T1.1, T1.2
fixture-ok: yes
owns:       src/canon/stage2.rs
status:     done
```
Inbound `Dependency`/`Causal`/`Hierarchical` sources tracing to ≥3 distinct interactions
spanning ≥0.3 of session temporal extent — via `store.interaction_span()`. Only edges and
interactions older than `canonization_edge_min_age=60s` count (**the adversarial inflation
guard — test it adversarially**: a burst of fresh edges must not promote). Venerable ⇒
eviction-immune (T4.5 already excludes; assert the wiring).

**Done when:** the fixture's stage-2-pass/stage-3-fail node goes Venerable and a
freshly-inflated twin does not.

---

### T6.3 — Stage 3: Canonical
```yaml
requires:   T1.1, T1.2
fixture-ok: yes
owns:       src/canon/stage3.rs
status:     done
```
`store.blast_radius() > 5` hypothetical-removal orphans. Re-promotion blocked for
`canonization_repromotion_cooldown=300s` after any demotion (`last_demotion_time`).

**Done when:** the planted pillar promotes, the sub-threshold node doesn't, and a
just-demoted node is refused until cooldown lapses (mocked clock).

---

### T6.4 — Evaluation loop, budget, demotion, audit ★
```yaml
requires:   T6.1, T6.2, T6.3, T4.6
fixture-ok: yes
owns:       src/canon/mod.rs
status:     done
```
Every `canonization_eval_interval=60s`: ≤ `canonization_eval_batch_size=50` Venerable nodes
per cycle, round-robin cursor, score-descending within batch (anti-starvation preserved).
Budget: `max_canonical_nodes=1000`, lowest-blast-radius demoted first. Demotion sets
`last_demotion_time`, nulls `blast_radius`, writes `canonization_events`. **Every**
transition (up and down) goes through `store.record_canonization()` and emits
`DaemonEvent::Canonized` — the audit trail is what the CockroachDB MCP server shows on
screen in the demo (spec §13 step 5); an unrecorded transition is a demo bug.

**Done when:** fixture session progresses None → Candidate → Venerable → Canonical across
simulated eval cycles with a `canonization_events` row per hop, and budget overflow demotes
the lowest blast radius with a recorded event.

---

## Exit criteria

- [x] Full three-stage progression reproducible in one test (mocked clock, MemoryStore)
- [x] Same test green against SQLite once T3.6 lands (the swap proves the abstraction)
- [x] Inflation guard and cooldown adversarially tested
- [x] Audit trail complete: transitions in test == rows recorded

---

## Handoff Log

- **Branch:** `phase/p6-canonization` (serial, task branches merged: t6.1, t6.2, t6.3, t6.4, phase-r1).
- **Shape:** `src/canon/{mod,stage1,stage2,stage3,eval}.rs`. Stages are store-agnostic
  `&impl GraphStore` predicates; `eval.rs` is the write path (one hop/cycle, budget,
  audit, emit).
- **Stage 1** (`stage1_candidates`): session gate (>= min_peer_count non-Canonical),
  `gc_survived >= 3`, score strictly above nearest-rank P90. Output NodeId-ascending.
- **Stage 2** (`stage2_passes`): `interaction_span` distinct>=3 && coverage>=0.3, min_age
  forwarded (inflation guard).
- **Stage 3** (`stage3_passes`): `blast_radius > 5` (u64, CON-6 try_from) && not inside
  `last_demotion_time + cooldown`.
- **T6.4** (`Evaluator::eval_cycle`): Stage 1 -> Stage 2 -> Stage 3 (capped at remaining
  budget), then `demote_over_budget`. Every hop: graph.apply -> store.record_canonization ->
  emit_canonized. `hopped` set enforces one hop/cycle (None->Candidate->Venerable).
- **Surprises / decisions:**
  - `demote_over_budget` originally used a hopped-skip; phase review found it could
    under-demote, so Stage 3 promotions are now **capped at the remaining budget**
    (a Venerable that would overflow waits; no same-tick promote-then-demote).
  - MemoryStore `apply_mutation` de-dupes `canonization_events` on `event.id` (same
    contract as SQL `ON CONFLICT (id) DO NOTHING`) so the dual-write (apply mutation +
    record_canonization) never duplicates the demo artifact on flush.
  - SQLite audit reload orders by `occurred_at, id`; advance the injected clock per cycle
    in tests so hop order is stable.
- **Exit criteria:** all 4 met (3-hop progression on MemoryStore AND SqliteStore; inflation
  + cooldown adversarial; audit rows == transitions).

## Phase review (serial close)

- **R1** (`adve-review-p6-canonization.md`): REQUEST CHANGES — 2 P1 (same-cycle demote,
  MemoryStore audit duplicate). Fixed `20f88a6`.
- **R2** (`adve-review-p6-canonization-r2.md`): REQUEST CHANGES — 1 P2 (budget under-demotes
  when same-cycle promotions exceed budget). Fixed `b48ec05` (cap Stage 3 at remaining budget).
- **R3** (`adve-review-p6-canonization-r3.md`): ACCEPT — clean.

Gates: fmt clean; clippy `-D warnings` clean; default 461/0; `store-sqlite` row 494/0;
no-default check clean.

## Branch-level adversarial review (fable ×5, 2026-08-13) — CLOSED

`adve-review/adve-review-p6-canonization-fable.md`: 19 findings (2 P1 / 5 P2 / 12 P3),
remediated in 2 opus rounds (12 commits, `06fcc00..d3bdc6b`), round-3 verify **CLEAN**.
The P1s reshaped T6.4: the eval loop is now real (`canon::CanonizationTask`, FlushTask-
shaped, consumes `canonization_eval_interval`; `eval_cycle` is gather-before-lock over
`&RwLock<Graph>`) and the Stage-3 cursor is identity-anchored (churn/starvation tested).
Notable semantic changes vs the close notes above: the `hopped` set is gone (one hop per
cycle is structural — disjoint pre-cycle status windows); the three canonization columns
have a **single writer** (UpsertNode preserves them on conflict, all 3 backends); recall
orders Canonical members first (spec §10); `now` is injected through
`interaction_span`/`blast_radius`. Residual P3s + P8 checklist items (F18, R3-1, scale
note) recorded in the review disposition and in PHASE-8-surface.md.

Gates at close: fmt clean; `-D warnings` check clean ×3 feature sets; tests 490/0
(default), 527/0 (store-sqlite), 513/0 (store-cockroach); zero test removals.
