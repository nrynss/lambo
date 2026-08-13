# Deep Adversarial Review — P6 Canonization (`phase/p6-canonization` @ `2cdb7a6`)

**Reviewer:** ReviewP6 (independent phase pass)
**Mandate:** attempt to BREAK the per-task ACCEPT claims (T6.1–T6.4). Probe cross-stage skips, dual-write/flush audit, daemon wiring, last_gc, CON-6, fixture rewind honesty, one-hop, test honesty. READ-ONLY.

---

## Verdict: **REQUEST CHANGES**

T6.1–T6.3 predicates hold. T6.4's one-hop state machine and durable audit trail do not. Two P1 defects are reachable from the code T6.4 shipped: a node can take two legal edges in one `eval_cycle`, and `commit_transition`'s dual-write duplicates `canonization_events` on the MemoryStore flush path the rest of the tree already runs.

---

## Findings (severity-ordered)

### F1 — [P1] Same-cycle Stage 3 promotion then budget demotion violates one-hop

- **Location:** `src/canon/eval.rs:199-234` and `src/canon/eval.rs:312-328`
- **FACT:** Module docs (`eval.rs:3-4`) lock "at most one legal edge of the state machine per tick." Stage 1 and Stage 2 insert into `hopped` so a just-promoted node is not re-checked. Stage 3 promotion (`eval.rs:228-230`) does **not** insert into `hopped`. `demote_over_budget` then walks every current Canonical, including nodes this cycle just wrote `Venerable → Canonical`, and will emit `Canonical → None` when `canonicals.len() > max_canonical_nodes`.
- **Trigger:** session already at `max_canonical_nodes`; a Venerable node clears Stage 3; its store `blast_radius` is the lowest among Canonicals (or tied lowest by NodeId). Both hops commit, both emit, both land in `canonization_events`.
- **Impact:** the node is Canonical only for the rest of the same function call, then returns to `None` with `last_demotion_time = now`. Re-promotion is blocked for `canonization_repromotion_cooldown` (300s). Under the exact pressure the budget exists for, a newly earned pillar is discarded and cooled.
- **Test gap:** `one_hop_per_cycle_none_with_stage2_evidence_becomes_candidate` only locks Stage 1 vs Stage 2. `budget_demotes_lowest_blast_and_records_demotion` starts from already-Canonical nodes. Nothing constructs "promote then overflow."
- **Direction:** `hopped.insert(id)` after a Stage 3 commit, and skip `hopped` ids in `demote_over_budget` (or snapshot the Canonical set before Stage 3). Add a test: `max_canonical_nodes = 1`, one existing Canonical with blast 8, one Venerable that clears Stage 3 with blast 6 — after one cycle the new node must still be Canonical (or still Venerable if the policy is "don't promote into overflow"), never `None` with a same-tick demotion.
- **Confidence:** 0.95

### F2 — [P1] Dual-write duplicates MemoryStore audit rows on flush

- **Location:** `src/canon/eval.rs:271-281` (`commit_transition`) consuming `Graph::apply_canonization_transition` (`src/graph/graph.rs:621-622`) and `GraphStore::record_canonization`
- **FACT:** every hop does all three of: (1) RAM apply, which **always** appends `Mutation::CanonizationTransition` to the write-behind log; (2) `store.record_canonization`, which on MemoryStore is `apply_mutation` of that same event (`src/store/memory.rs:479-487`) and **unconditionally** `canonization_events.push` (`memory.rs:170`); (3) `emit_canonized`. `FlushTask` (`src/store/flush.rs:319-321`) later `drain_log()`s that mutation and `flush`es it. SQLite/Cockroach `INSERT … ON CONFLICT (id) DO NOTHING`, so SQL adapters stay at one row. MemoryStore has no id check.
- **Trigger:** `eval_cycle` then any flush of the drained log against `MemoryStore` — the P8 T8.1 assembly (`dev-diary/PHASE-8-surface.md`: Memory builder wires graph + daemon + flush) and any test that `drain_log` + `flush` after eval.
- **Impact:** `load_session().canonization_events` is twice the committed hops. The demo artifact T6.4 exists to keep (`canonization_events`, spec §13 step 5) lies: one RAM hop, two durable rows, one emit. P6's own tests never flush after `eval_cycle`, so `audit_rows_equal_committed_transitions` and the three-hop fixture test stay green.
- **This is introduced by T6.4.** MemoryStore's push-always apply is older; the dual-write that feeds the same event through both `record_canonization` *and* the mutation log is new.
- **Direction:** pick one durable path. Either stop calling `record_canonization` and let flush carry the mutation (demo then waits `backend_flush_interval`), or make `MemoryStore::apply_mutation` skip an event whose `id` is already in `snap.canonization_events` (SQL parity), or drain the just-appended mutation inside `commit_transition` after a successful `record_canonization`. Add a test that `eval_cycle` then `store.flush(&graph.drain_log())` and asserts `store` event count == `outcome.transitions().count()`.
- **Confidence:** 0.93

---

## Per-task ACCEPT claims — disposition

**T6.1 Stage 1 — HOLDS.** Peer gate is `peers.len() < min_peer_count` (19 closed, 20 open). Nearest-rank P90 is `ceil(0.90 n)` with a lock against R7 interpolation. Exactly-at-P90 fails (`>`). `gc_survived < 3` fails. Canonicals are neither peers nor candidates. Missing `ScoreTable` entry is `0.0`. Output is NodeId-ascending. Fixture test rewinds planted Canonical `user schema` *after* asserting the plant — honest.

**T6.2 Stage 2 — HOLDS.** Predicate is `distinct >= 3 && coverage >= 0.3` on `store.interaction_span`. Status is not consulted (T6.4 sequences). Inflation test at `min_age = 60s` vs `0` discriminates. Eval only feeds already-Candidate, non-`hopped` nodes, so Stage 2 does not run on `None` and does not skip Candidate.

**T6.3 Stage 3 — HOLDS.** Strict `blast > 5` as `u64` (CON-6: no `as i32` in the predicate). Cooldown is `now < last_demotion + cooldown`; `None` is not a cooldown; `now == last + cooldown` passes. Fixture pillar blast 8 passes, `api layer` blast 1 fails, just-demoted pillar refused.

**T6.4 eval — BROKEN by F1 and F2.** One-hop holds for Stage 1→2 and Stage 2→3. It does not hold for Stage 3→budget. Dual-write + emit is tested only before flush. CON-6 narrow at the Stage 3 write gate (`narrow_blast_radius` / `i32::try_from`) is correct and tested.

---

## Probe log (asked-for seams)

### Cross-stage skip / Stage 2 on None / one-hop
Eval Stage 1 only commits `None → Candidate`. Stage 2 iterates `status == Candidate && !hopped`. Stage 3 iterates `status == Venerable && !hopped`. A None node with Stage 2 evidence becomes Candidate and stops (`one_hop_per_cycle_…`). The remaining hole is F1 (Stage 3 + budget), not a stage skip to Venerable/Canonical.

### Dual-write / flush
See F2. SQL adapters are idempotent on event id. MemoryStore is not. Graph-first then record is the documented recovery story (store outage leaves the mutation in the log). Combining that with `record_canonization` *and* leaving the mutation queued is what doubles the MemoryStore trail.

### Eval never wired into Daemon loop
**P8, not a T6.4 product bug.** `run_cycle` (`src/daemon/mod.rs:699-893`) is synchronous: rescore, detect, publish, GC. No `Evaluator`, no `eval_cycle`, no `canonization_eval_interval`. That matches T4.6 docs (`events.rs:52-54`: daemon does not fabricate `Canonized`) and eval's own contract (`eval.rs:35-37`: caller must not hold `RwLock<Graph>` across the async call; tests pass `&mut Graph`). PHASE-8 T8.1 is the assembly (`graph + daemon + flush`). PHASE-6 "every 60s" is not live until that caller exists. Not a silent drop of a new event kind — there is no in-tree dispatcher that should be routing `eval_cycle` yet.

### `last_gc.canonical_over_budget` unused
**Not a defect.** PHASE-4 told T6.4 to read `Daemon::last_gc()`. T6.4 instead recounts live Canonicals on the graph (`eval.rs:293-300`). That is the fresher signal (GC may not have run; `last_gc` can be `None`). Budget enforcement does not depend on the GC flag.

### SQLite exit criterion
PHASE-6 still has `- [ ] Same test green against SQLite once T3.6 lands`. T3.6 has landed; no `stage*_passes` / `eval_cycle` test uses `SqliteStore`. Abstraction is unproven at phase close. Coverage hole, not a runtime bug in the shipped predicates. Not raised as a blocker.

### Lock across store awaits
`eval_cycle` takes `&mut Graph` and `.await`s store queries between hops. Documented. No production caller holds a `parking_lot` guard across it. P8 must spawn eval without nesting that borrow under `graph.write()` for the whole cycle (brief write for each `commit_transition` only). Seam, not a P6 bug.

### CON-6
Stage 3 compares `u64`. Write gate is `i32::try_from` → `StoreError::Invariant`, not `as i32`. `blast_radius_narrow_rejects_unrepresentable_u64` discriminates wrap-to-`i32::MIN`. Holds.

### Fixture planted Canonical / rewind honesty
`session-rest-api` plants `user schema` Canonical (blast 8) and `api layer` Venerable (blast 1), `canonization_events: []`. Stage 1 asserts the plant, then rewinds. Eval `rewind_canonicals` only flips Canonical → None (does not invent hops). Three-hop test then requires the three pairs on the in-graph audit, the store, and `emit_canonized`. Honest.

### Determinism / NaN / empty sessions
Stage 1 sorts peer scores with `total_cmp` and compares with `>` (IEEE: `NaN > p90` is false). Empty / below-gate sessions return `[]`. Stage 3 batch is score-desc / NodeId-asc via `total_cmp`. Empty venerable ring is a no-op. No finding.

### Test honesty (would a missing emit still pass?)
No. `audit_rows_equal_committed_transitions` and `rest_api_user_schema_progresses_three_hops_with_audit` compare drained `Canonized` events to committed hops. `failed_apply_does_not_record_or_emit` requires an empty drain. Removing `emit_canonized` fails those tests. What they do *not* catch is F1 (no promote-then-demote case) and F2 (no post-eval flush).

---

## Confidence

Overall: **0.90** — F1 is a direct reading of the hop set vs budget loop; F2 is a direct reading of apply + record + MemoryStore push + FlushTask drain. Predicates and CON-6 hold. Daemon wiring / last_gc / SQLite swap are phase-boundary, not silent mis-implements of T6.4's owned surface.
