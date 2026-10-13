# #20: evidence-driven supersession

Recorded 2026-10-13 for 0.6.0. Branch `feat/20-supersession` from `main`
`40c09d98`. Design of record: issue #20, its Phase 0 design refresh
(2026-10-12) and the owner's approval of its recommendations (age-eligible
dependent evidence; cooldown on both sides; supersession committed before
promotions with the audit in actual commit order; a separate historical
`superseded` annotation after demotion; cross-session supersession stays
refused). Builds on #36 (dependents, the 14-day cap, the shared fair-test
fixture) and #19 (a) the edge, (b) `against`, (c) the warning and its
dating ([feature-19c-contradiction-warning.md](feature-19c-contradiction-warning.md)).

## What it adds

- `Config::canonization_supersede` / top-level `lambo.toml`
  `canonization_supersede` (default `false`), `EvalParams::supersede`.
- `canon::supersession`: probes, verdicts, the apply-time re-check, the
  hold. `EvalOutcome::supersessions`; `transitions()` in commit order.
- `CanonizationEvent::reason: Option<DemotionReason>` (`Budget`,
  `Superseded { by }`), `demotion_reason()`, `check_reason()`; nullable
  `reason` / `superseded_by` columns in all three init schemas.
- `GraphStore::latest_structural_evidence_at` and
  `GraphStore::interaction_span_since` (default `Capability`), implemented by
  MemoryStore, SQLite, the Postgres family and the Tiered wrapper over one
  shared reading, `store::evidence`.
- Recall's `superseded` annotation (`recall::format::superseded_warning`);
  inspect's `last demotion` line (`canon::with_last_demotion`).

## The rule

A Canonical `C` is superseded by `S` when, at the cycle's `now`:

1. `S -Contradicts-> C` exists, written by `now` (`contradiction::written_by`),
   `S` a concept of the same session (cross-session edges are refused at
   write time, and the probe checks the session again).
2. `S`'s eligible dependent evidence strictly after `C`'s watermark clears
   Stage 2's predicate (`stage2_evidence_passes`): at least three origin
   interactions other than `S`'s own, coverage at least 0.3 of
   min(session extent, `canonization_coverage_window`).
3. `C` is outside its re-promotion cooldown, and so is `S`.

`C` goes `Canonical -> None`, `blast_radius = None`,
`last_demotion_time = now`, `reason = Superseded { by: S }`. `S` is not
promoted. Competing sources for one `C`: newest eligible evidence (the
watermark accessor on `S`), then canonical key, then id; one reason per
target.

## Decisions

### One clock with #19(c) (reconciling Phase 0)

Phase 0 described the watermark on "the eligible dependent origin
interaction's about-time". #19(c) then settled evidence differently, and
the brief says to follow #19(c) where they differ. So:

- evidence is one **dependence edge** (as #19(c)), not one origin
  interaction;
- eligible by **write time** (`created_at <= now - min_age`, #19(c) review
  M3), not about-time; a cutoff out of range makes nothing eligible;
- dated by `declared_at` (about-time clamped to write time);
- own-origin dependents excluded;
- `C`'s watermark is the later of its origin's clamped about-time and its
  latest eligible evidence, **excluding** #19(c)'s self-suppressed evidence:
  edges written by the contradiction's declarer, and `S`'s own dependence on
  `C`. A writer cannot defend a belief against its own contradiction.

Phase 0's "both edge and origin strictly after the cutoff" is kept for
`S`'s side: a new edge from an old concept (origin before the watermark)
does not count, and a backdated new edge does not either. Origins must be
interactions of the session written by the cutoff; the spread is measured
on their clamped dates; the denominator is Stage 2's own session extent.
The cutoff is applied before origins are counted and spread measured.

Derived to dogfood memory.

### Store surface

Each adapter only fetches raw rows (`store::evidence::EvidenceRows`: one
row per dependence edge with its dates and its dependent's origin dates,
the node's own origin, the session extent); the rules live once in
`store::evidence` (`latest_at`, `span_since`), which also builds the same
rows from the in-RAM graph (`rows_from_graph`) for the apply-time
re-check. Three SQL dialects re-deriving the clamp, the eligibility and the
exclusions would drift; this way parity is structural. The shared case
`test_util::supersession` pins the answers on MemoryStore, SQLite, Tiered
and the graph, and by hand on Postgres (`store::pg::supersession_live`,
ignored) and Cockroach (`conformance_suite`).

The exclusions (self-suppression) need write-time attribution, which only
the graph has (`daemon::conflict::WriterTimeline`), so the evaluator
computes them and passes edge ids to `latest_structural_evidence_at`.

### Order, budget and fairness

Supersession commits first, then Stage 1, 2 and 3 against the recomputed
budget, then budget demotions. Stage 3 is gathered at a full budget
whenever supersession probes are pending (otherwise the freed slot could
never be spent that cycle). A superseded concept takes no other hop that
cycle. `EvalOutcome::transitions()`, phase 4's `record_canonization` calls,
the in-graph audit and the write-behind log all follow commit order.

Probes are bounded: a ring of at most `batch_size` Canonical targets with
an identity cursor, like Stage 3's, each probing all its live
contradictions.

The budget cut used to take the first `overflow` ranked concepts and skip
the ones no longer Canonical, under-demoting; it now passes over them and
demotes the next Canonical one. Pre-existing in shape (a concurrent
change), made reachable by supersession; fixed in the same commit.

### Concurrency

Probes run with no lock. Apply, under the write guard, re-checks from the
graph: target Canonical and outside its cooldown, source a concept of the
session outside its cooldown, the edge still `S -Contradicts-> C` and
written, the graph's watermark for `C` not newer than the one the store
used, and `S`'s graph-side span after that watermark still passing. Any
failure skips the verdict (debug log). Hybrid's `plan_still_holds` is not
used. Canon transitions stay outside `Graph::transaction`.

### Reciprocal contradictions and cooldown

Verdicts apply in target-id order. When `C` and `S` contradict each other
and both qualify (possible with self-suppression), the first demotion
stamps its concept's cooldown, so it can no longer supersede the other in
the same cycle; inside the cooldown nothing flips; after it the other side
may win. Pinned by `reciprocal_contradictions_flip_at_most_once_per_cooldown`.

### The hold (not in Phase 0: a conservative choice, for the owner)

Without it, a superseded concept whose own evidence still passes every
stage climbs back (Stage 1 at once, Venerable, Canonical once its cooldown
is over) and is superseded again: a flap once per cooldown window,
forever, and a fixed point is never reached. The issue's intended steady
state is that the side with the newer evidence wins. So, with
supersession on, Stage 3 also probes its window's Venerables with the same
predicate and does not promote one that would be superseded at once; it
waits at Venerable until it earns newer evidence. Derived to dogfood memory.
If the owner prefers the flap, the hold is one `gather_holds` call and one
`held` check in `eval.rs`.

### The audit

`reason` is demotion-only. A historical `NULL` pair reads as `Budget` on a
demotion and as no reason on a promotion; budget demotions keep writing
`NULL`, so their rows and the default-off goldens are byte-identical to
before. An explicit `budget` is accepted on read. Malformed pairs (budget
with an id, superseded without one, an id alone, unknown text, a reason on
a promotion) are refused on load, and every write gate (graph, MemoryStore,
SQLite, the Postgres family) refuses a reason on a promotion and a
self-supersession. `superseded_by` is a bare id, no foreign key.

Rows of one cycle share `occurred_at`, and the stores reload by
`(occurred_at, id)`, so a reload orders one cycle's rows by id, not by
commit order. Each concept takes at most one hop per cycle, so nothing
reads that order; commit order is what the writes, the in-graph audit and
the outcome carry.

### Capability

A store without either accessor answers `Capability`: supersession (and
the hold) are skipped for the cycle with one
`CanonizationSupersessionUnsupported` warning; the cycle runs. Any other
store error keeps the evaluator's established semantics (Stage 1 lands
alone, the cycle fails with its partial outcome).

### Recall and inspect

The live #19(c) `contradiction` annotation needs a Canonical or Venerable
target and lapses at `None`. A hit whose **latest** demotion was a
supersession and that is not Canonical again carries `superseded` instead,
with `⤓ Superseded at <time> by "<excerpt>" (id <short>)`; the quote drops
once the source is collected. It is history: not a hot-list condition, not
forced, not past `top_k` or a layer floor, and it counts toward
`max_tokens`. A later budget demotion replaces it. Inspect's canonization
section ends with the latest demotion and its reason; this line also shows
for budget-demoted concepts with supersession off (a visible, additive
change, in the CHANGELOG).

### Dry run

`lambo canon dry-run` runs with supersession off (its `EvalParams` default);
a library caller can set `eval.supersede`. Not wired to `lambo.toml`.

## Tests

| Case | Test |
|---|---|
| Newer support demotes; reason, stamp, S not promoted | `canon::eval::tests::supersession::newer_support_for_the_contradicting_concept_demotes_the_canonical_one` |
| Default off changes nothing | `default_off_changes_nothing`, fixture expectations, every existing golden |
| Assertion only | `an_assertion_alone_never_demotes`, fixture `lease_revoke` |
| Older support, cutoff before count, equality | `older_support_does_not_count`, `the_cutoff_applies_before_origins_are_counted`, `support_at_the_watermark_instant_does_not_count`, fixture `ttl_sliding` |
| Fresh (age floor), future/backdated, foreign, own-origin, Hierarchical | `fresh_support_counts_only_once_it_is_old_enough`, `store::evidence::tests::*`, `test_util::supersession` (all stores and the graph) |
| Self-suppression | `the_declarer_cannot_defend_against_its_own_contradiction`, `the_contradicting_concept_citing_its_target_is_not_evidence` |
| Competing sources | `the_source_with_the_newest_evidence_wins` |
| Cooldowns, reciprocal | `a_target_inside_its_cooldown_is_not_superseded_again`, `a_source_inside_its_cooldown_cannot_supersede`, `reciprocal_contradictions_flip_at_most_once_per_cooldown` |
| The hold | `a_venerable_that_would_be_superseded_is_held_at_venerable`, fixture `supersession_on_the_fixture` |
| Full budget, slot reuse, commit order | `a_freed_slot_is_spent_in_the_same_cycle_in_commit_order` |
| No same-cycle re-promotion | `a_superseded_concept_is_not_re_promoted_in_the_same_cycle` |
| Concurrent change | `apply_rechecks_a_graph_that_changed_since_the_probe` (newer evidence, edge removed, source collected, target demoted) |
| Capability | `a_store_without_the_accessors_skips_supersession_with_one_warning`, `store::span::tests::the_supersession_defaults_fail_closed` |
| Fencing, retry, reopen | `a_failed_audit_write_keeps_the_supersession_and_the_flush_records_it`, fixture reload check |
| Legacy and malformed rows, upgrade | `store::sqlite::tests::demotion_reason::*`, `test_util::supersession::check_reason_persistence` (Memory, SQLite; Postgres and Cockroach by hand), `types::tests::demotion_reason_columns_and_wire_shape`, `graph::...::the_write_gate_checks_the_demotion_reason` |
| End to end on the fixture | `canon::fair_test_tests::supersession_on_the_fixture` (both policies, Memory and SQLite; live legs in `fair_test_live`) |
| Recall and inspect | `memory::tests::supersession::*` |
| Config | `config::tests::canonization_supersede_defaults_off_and_parses_from_the_file`, `resolve::tests` |

The fixture gained `retry_forever` / `retry_bounded` (earned Canonical, then
superseded when supersession is turned on after a fixed point reached
without it; the hop sequence is asserted) and `ttl_fixed` / `ttl_sliding`
(support mostly older: never superseded). The dry-run golden and the tests
counting the fixture's Canonicals moved by exactly those rows (4 to 6
Canonical); no existing row's outcome or line changed.

Live Postgres and Cockroach legs (`supersession_live`,
`conformance_suite`, `fair_test_live`) were written but not run (manual by
owner decision).

## Spec errata

**§10 canonization.** A second demotion path, supersession, before
promotions, off by default; the hold at Stage 3 while supersession is on;
`CanonizationEvent` gains a demotion reason.
