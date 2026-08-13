# Adversarial Review — P6 Canonization, Round 2

Target: `phase/p6-canonization` @ `8be251a` (remediation `20f88a6`).

Verdict: **REQUEST CHANGES** — both P1 fixes hold, but the same-cycle-demote
remediation introduced a budget-violation edge case (P2, below).

## Prior P1s — verified fixed

### P1-1 same-cycle demote
`eval_cycle` Stage 3 now does `hopped.insert(id)` after promotion, and
`demote_over_budget` receives `&hopped` and skips hopped ids via
`ranked.retain(|(_, id)| !hopped.contains(id))`.

In the common case (`h <= max_canonical_nodes`, where `h` is this cycle's
Stage-3 promotion count) the math is correct: `overflow = C - max`, non-hopped
count is `C - h >= overflow`, so exactly `overflow` pre-existing Canonicals are
demoted and the final count is `max`. The regression test
`just_promoted_canonical_not_demoted_in_same_cycle` discriminates (on unfixed
code the just-promoted node 20, blast 6, would be the victim; the test asserts
node 10 is demoted instead).

### P1-2 audit duplicate
`MemoryStore::apply_mutation` now skips `canonization_events.push` when an
event with the same `event.id` already exists, matching SQLite's
`ON CONFLICT (id) DO NOTHING`. `event.id` is a fresh `NodeId::new()` UUID per
transition, so the only possible collision is the write-behind replay of the
same committed transition (`record_canonization` immediate write + later
`flush` of the drained log). Correct. The O(n) `any` scan per mutation is O(n²)
overall, but this is the test/fixture store and correctness is the criterion.

`flush_after_eval_does_not_duplicate_audit_rows` discriminates: without the
dedup, `drain_log()` + `flush` re-appends the `CanonizationTransition`
mutation, yielding 2 rows vs. the 1 committed.

### No compile concern introduced
The remediation adds no imports (`HashSet`/`HashMap` already imported in both
files; `HashMap` in `memory.rs` is used by `blast_radius`). No new dead code.

## New finding (P2)

**Budget demotion under-demotes when same-cycle promotions exceed
`max_canonical_nodes`.**

`demote_over_budget` measures `overflow` against *every* Canonical (including
this cycle's Stage 3 promotions) at line 317, *then* removes the hopped ids at
line 318. `take(overflow)` at line 319 can therefore only demote the `C - h`
non-hopped Canonicals. If `h > max_canonical_nodes` — reachable whenever
`canonization_eval_batch_size > max_canonical_nodes`, or more generally
whenever more Venerable nodes clear Stage 3 in one cycle than the budget
allows — the final Canonical count is `h > max`, the spec §10 bound
("bounded by max_canonical_nodes") is violated, and in the extreme case where
every Canonical is a same-cycle promotion, nothing is demoted at all. Before
this remediation the budget always converged to exactly `max`.

Default config (`batch_size=50`, `max_canonical_nodes=1000`) cannot trigger
this, which is why the two regression tests (max=1, one promotion) pass, but
`EvalParams` is a public API with no `batch_size <= max_canonical_nodes`
validation.

Fix options: compute `overflow` against the non-hopped count (demote hopped
nodes only as a last resort when non-hopped count is insufficient), or cap
Stage 3 promotions at the remaining budget so `h` can never exceed it.

## Scope note
The working tree advanced to `phase/p7-embeddings` mid-review; this review is
anchored to commit `8be251a`, which remains intact in history.
