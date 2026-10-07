# Issue #29 — GC time bound, durable sweep mark, step-2 protections

Status: implemented on `fix/29-gc-time-bound` (from `main` c83b933, rebased on
f65f610), review round 1 remediated (see "Remediation" below); round 2 applies
two operator decisions (Observation exempt from the score cut, 365-day window)
and the round-1 review findings (see "Remediation, round 2"). Companion to
issue #29's dry-run comment and to #17
(`dev-diary/adversarial-review/adve-review-issue-17-canonization-reachability.md`).

## What changed

| Piece | Where |
|---|---|
| `GcMark { last_gc_epoch, last_gc_at }` is graph state, stamped on every `MutationBatch` by `drain_log`, carried by the flush loop, persisted as `sessions.last_gc_epoch` / `sessions.last_gc_at` with a field-wise monotonic max inside the flush transaction, returned by `load_session`, resumed by `Graph::from_snapshot` | `src/types/mod.rs`, `src/graph/graph.rs`, `src/store/{flush,memory,sqlite}.rs`, `src/store/pg/{mod,cockroach}.rs`, `migrations/*/001_init.sql` |
| The daemon reads and writes the watermark on the graph (no more `CycleState::last_gc_epoch`); drains advance it through `Graph::exempt_from_gc_measure` under the drain's own guard; a sweep records `(epoch_after, now)` under the sweep's guard | `src/daemon/mod.rs` |
| Trigger: `gc::sweep_due` — `gc_interval` mutations since the mark, or `gc_max_interval` elapsed since `last_gc_at` with at least `gc_idle_floor` mutations since the mark | `src/daemon/gc.rs`, `src/config.rs` |
| Step 2: Logic/Constraint/Observation exempt from the score cut (`ConceptType::exempt_from_gc_score_cut`); GC's eviction recency is time since last touch over a 365-day window; collections capped at `max(32, 5%)` of unprotected concepts per sweep, reported in `GcOutcome` | `src/daemon/gc.rs` |
| Config: `[daemon] gc_max_interval_secs` (86 400) and `gc_idle_floor` (100); zero refused by `validate()`; unknown keys still hard errors; no env overlay (none of the `[daemon]` keys has one) | `src/config.rs`, `lambo.example.toml`, `docs/reference/{config,api}.mdx` + site copies |

## Decisions and why

1. **Step-2 protection: option (a), exempt Logic and Constraint from the score
   cut; Observation joined them in round 2 (operator decision, 2026-10-07).** Option (b), "below the bar on two consecutive sweeps", does not protect
   anything on an idle store: a concept's score does not change between sweeps
   without writes, so the second verdict is the first one a day later, and it
   needs a persisted per-concept counter to do even that. The exemption is a
   hard guarantee, independent of whether #30's frequency dimension is live.
   Orphan and disconnected-component cleanup still apply to both types; they are
   structural, not age-based.
2. **Eviction recency = `1 − age / 365 days`, clamped, age = now − max(created_at,
   last_accessed). GC's cut only.** `score::rescore` (daemon ranking, recall,
   Stage 1's P90) keeps span-relative recency: changing it would move recall
   ranking and canonization, which #29 does not set out to change, and nothing
   showed the two must share one definition. The window is a const
   (`GC_RECENCY_WINDOW`), not a `[daemon]` key: it is a scoring constant, and
   `config.rs` keeps scoring bars out of the file. The window was 90 days
   (chosen from the sensitivity table below) and became 365 by operator
   decision in round 2: Lambo is long-term memory, the at-risk set is mostly
   long-tail pointers (paths, PRs, commits, verdicts), and at ~4% of the store
   the space a short window saves is negligible next to what losing one costs.
3. **The mark is a batch stamp, not a `Mutation` variant.** Same mechanism and
   same argument as #17's `mutation_epoch`: a mutation would bump the epoch it
   is measuring, needs adapter match arms, and is not graph content. The two
   fields merge independently (max of each); both only move forward within a
   writer, so the independent max is the pair's true newest value and a clock
   that went backwards can only delay the next timed sweep.
4. **A never-swept session anchors its clock, it does not sweep.** On first
   observation the daemon sets `last_gc_at = now` (if unset). Upgrading a rig
   therefore never causes an immediate timed sweep; the first one comes a day and
   100 mutations later. The mutation trigger's #17 catch-up (watermark 0, lifetime
   epoch ≥ `gc_interval`) is unchanged for a never-swept session. If the anchor is
   not yet flushed when the writer restarts it re-anchors: a mark-only batch
   cannot persist, because sessions are resolved from mutations. For a
   never-swept session already over the idle floor this postpones the timed
   catch-up below until one write lands or the writer stays up for
   `gc_max_interval`; an idle store restarted more often than that never reaches
   it. The count-based #17 catch-up at `gc_interval` still fires.
   The anchor delays the timed trigger, it does not exempt the session: a
   never-swept session has `last_gc_epoch` 0, so the floor counts its whole
   lifetime, and one with at least `gc_idle_floor` mutations sweeps **once**,
   `gc_max_interval` after the anchor, even if it has been idle since. That is a
   deliberate catch-up for a store that grew before sweeps ran on time (pinned by
   `never_swept_session_over_the_floor_catches_up_once_after_the_anchor`); the
   sweep sets the mark, so it does not repeat.
   This is about **never-swept** sessions. A previously swept session is not
   swept *because* a writer attached, but if a sweep is already due by its
   stored mark (it took at least `gc_idle_floor` mutations since the last
   sweep and the writer was down past `gc_max_interval`), the first cycle
   after attach sweeps once. That is intended: it is the sweep the downtime
   delayed, not a restart artifact.
5. **Cap: `max(32, ceil(5% × unprotected))` over steps 2 and 3 together.**
   Structural garbage first: orphans, then the other disconnected components
   (both by id, measured on the post-step-1 graph), then score-cut candidates
   by ascending `score / bar`; concepts that the score cut's collections cut
   off from the chain (step 3's cascade) are collected in the same sweep with
   the budget left. Held-back candidates survive the sweep but do **not** take
   its `gc_survived` bump (the sweep judged them collectable; Stage 1 reads the
   counter), are listed in `GcOutcome::deferred` and counted once in
   `collections_deferred` (an orphan is also a disconnected component), and are
   re-evaluated next sweep. A
   bound cap is a `tracing::warn` via `GcOutcome::warnings`. 5% per day bounds a
   scoring mistake to a slow, visible erosion; the floor lets a small session
   clear its orphans.
6. **CoOccurrence margin: documented as intended, pinned by a test.** `derive`
   writes co-occurrence edges at exactly `MIN_EDGE_WEIGHT` (0.5), the cut is a
   strict `<`, and nothing decays weights, so step 1 never removes one. Cutting
   them would strip `lambo_derive` output of its only concept-to-concept links
   on the first sweep and cascade through density (321 of the 537 pre-change
   first-sweep collections touched a co-occurrence edge).
   `cooccurrence_edges_sit_exactly_on_the_step_one_bar_and_survive` fails if
   either constant moves.
7. **Survivor rows are still rewritten on every sweep.** `gc_survived` is a
   per-concept persisted counter that Stage 1 reads; skipping the bump on an
   "idle" sweep would change what the counter means, and deriving it from a
   session-level sweep counter changes the persisted format and Stage 1's input.
   What changed is how often it happens: the idle floor means a sweep needs at
   least 100 session mutations, so idle days rewrite nothing. A rig sweep
   rewrites ~3.2k concept rows in 500-row chunks (CONC-6/XP-10), at most once a
   day at human pace.

## Dry run (Metal rig snapshot copy; aggregates only) — historical, 90-day window, Observation under the cut

The tables from here to "Remediation (review round 1)" are the **90-day,
Observation-in-the-cut** measurements and are kept as the record of how the
design got here; the current numbers are in "Remediation, round 2".

Store: 3,370 concepts (Constraint 323, Entity 557, Logic 559, Observation 467,
Resource 1,464), 8,121 edges, 1,147 interactions, spanning 2026-08-19 to
2026-10-07; 0 protected; `access_count` 0 everywhere. "Before" is the #29 dry
run on main 078dccf; "after" is this branch, `now = 2026-10-07T06:30Z`, default
weights. Copy provisioned with this branch's binary (columns converged; mark
unset).

| | before | after |
|---|---|---|
| first sweep, collected | 537 (Observation 324, Resource 171, Logic 42) | **159 (Observation 159)** |
| first sweep, cap | none | 169, not bound |
| successive sweeps, no writes, same clock | 537, 22, 9, 2, 3, 1, 0… (574) | 159, 4, 0… (163, all Observation) |
| successive sweeps, no writes, +1 day each | identical to same clock | 159, 15, 20, 27, 13, 12, 3, 0, 7, 20, 23, 18 (317 over 12, all Observation; cap never bound) |
| Logic / Constraint collected, any scenario above | 42+ Logic | **0 / 0** |

Projection: one interaction appended N days after the last, no new concepts,
uncapped (the before column advanced the clock with the span).

| N | before (span + clock) | after, span + clock | after, span only (clock fixed) | after, capped first sweep |
|---|---|---|---|---|
| 0 | 537 | 159 | 159 | 159 |
| +7 d | 592 | 247 | 159 | 169 |
| +14 d | 650 | 333 (2 Resource) | 159 | 169 |
| +30 d | 745 | 588 (130 Resource) | 159 | 169 |
| +60 d | 1,049 | 1,211 (741 Resource, 3 Entity) | 159 | 169 |

Collection no longer grows with span alone. It does grow with time since last
touch, by design: that is the eviction criterion now.

Window sensitivity (uncapped first sweep at clock offset; 90-day-era
measurement, before the Observation exemption and the dependents rule):

| window | +0 d | +30 d | +60 d | +90 d | +120 d |
|---|---|---|---|---|---|
| 7 d | 1,324 | 1,468 | 1,468 | 1,468 | 1,468 |
| 30 d | 1,046 | 1,468 | 1,468 | 1,468 | 1,468 |
| 60 d | 353 | 1,179 | 1,468 | 1,468 | 1,468 |
| **90 d** | **159** | 588 | 1,211 | 1,468 | 1,468 |
| 120 d | 74 | 405 | 633 | 1,326 | 1,468 |

Every window converges on the same 1,468 concepts (Observation 467, Resource
995, Entity 6): the concepts whose structure alone (density, session activity,
edge bonus, type) is under their bar once recency is 0. The span-relative cut
converges on the same set as the span grows; the window changes the path, not
the destination. On an untouched store, a sparse Observation/Resource/Entity
is kept for about a quarter. Once #30 records accesses, a recalled concept
resets its own clock.

Rows rewritten by one sweep: 3,211 survivor upserts (of 3,703 mutations).

The tables above were measured before the review-round-1 remediation. The
1,468-concept convergence in particular no longer holds: the Resource
dependents rule keeps most of its 995 Resources (next section).

## Remediation (review round 1)

The numbers in this section are the round-1 state: 90-day window, Observation
still under the cut. Round 2's are below.

Changes, each its own commit: Resources with dependents are spared the score
cut (operator decision); GC's composite is the live-dimension score plus the
concept's own frequency term, so an access can only raise the accessed
concept's score (replaces ALGO-1's session-wide switch in GC's cut only);
`exempt_from_gc_measure` clamps to the epoch and its #30 advice is corrected;
a sweep time more than 5 min in the future is re-anchored and the regression
persists through a flagged store merge; the cap takes orphans, then
disconnected components, then the score cut; held-back candidates take no
survivor bump; the survivor drain order rotates per sweep; `lambo_stats` has a
`gc` block; a floor at or above `gc_interval` warns; restart/attach wording.

**Dependents rule, precisely.** A Resource is spared when another concept has a
`Dependency`, `Causal` or `Hierarchical` edge into it (record_action's
direction: action → depends_on / produces / modifies), or when its blast
radius is non-zero (it is the only structural source of some concept). The
second sense covers the action nodes themselves: incoming-only would collect
an action node, its targets would lose their only incoming edge, and the next
sweep would take them.

### Dry run (fresh copy of the same snapshot, `work29r.db`; aggregates only)

Same store, `now = 2026-10-07T06:30Z`, default weights, copy provisioned with
this branch's binary, mark unset.

Dependents census: 1,307 of 1,464 Resources have dependents — 940 by an
incoming edge, 373 by blast radius, 6 by both, so **367 are protected only by
the blast-radius sense** (an incoming-only rule would leave them to the cut).

| clock | capped first sweep | uncapped | Resources spared by the rule |
|---|---|---|---|
| +0 d | 159 (Observation) | 159 (Observation) | 0 |
| +7 d | 169, cap bound (78 deferred) | 247 (Observation) | 0 |
| +14 d | 169, cap bound (164 deferred) | 333 (Observation 331, Resource 2) | 0 |
| +30 d | 169, cap bound (304 deferred) | 473 (Observation 458, Resource 15) — was 588 | 115 |
| +60 d | 169, cap bound (390 deferred) | 559 (Observation 467, Resource 89, Entity 3) — was 1,211 | 652 |

Successive capped sweeps, no writes: same clock 159, 4, 0… (163, all
Observation); +1 day each 159, 15, 20, 27, 13, 12, 3, 0, 7, 20, 23, 18 (317,
all Observation) — unchanged, because nothing in the first 12 days reaches a
Resource with dependents.

**Untouched end state** (uncapped sweeps at +365 d until nothing more goes):
**602 collected — Observation 467, Resource 122, Entity 13** (was 1,468:
Observation 467, Resource 995, Entity 6). 884 Resources sit under their bar
and are kept only by the dependents rule; 1,309 Resources with dependents
remain. Logic and Constraint: 0 collected.

**Blast radius:** total over all concepts 1,165 before, 1,184 after the end
state (removing a second source can leave a concept with a single one, which
then counts); concepts above Stage 3's bar (blast radius > 5): **41 before, 41
after**.

**Synthetic access** (uncapped first sweep at +0 d; end state at +365 d;
"newly" = collected with access but not without):

| scenario | accessed | first sweep | newly | end state | newly |
|---|---|---|---|---|---|
| no access | 0 | 159 | — | 602 | — |
| one access on the hub Entity, at its creation time | 1 | 159 | **0** | 602 | **0** |
| one access on the hub Entity, at `now` | 1 | 159 | **0** | 602 | **0** |
| replay k=1 (#30 review method) | 2,115 | 68 | **0** | 543 | **0** |
| replay k=2 | 2,671 | 42 | **0** | 470 | **0** |
| replay k=4 | 3,065 | 19 | **0** | 324 | **0** |

Before the remediation one access on one Entity took the first sweep from 159
to 412 and the end state from 1,468 to 2,034. Now an access never adds a
collection anywhere; reads only save concepts (91 / 117 / 140 saved from the
first sweep for k = 1 / 2 / 4). Replay is the #30 review harness's method: at
each interaction, recall k queries built from the previous interaction's
derived concepts, every hit an access at that interaction's time.

## Remediation, round 2

Operator decisions, each its own labelled commit:

* **Observation is exempt from the score cut** (`feat(gc)!`). The exemption is
  `ConceptType::exempt_from_gc_score_cut`, an exhaustive `match`, replacing the
  hard-coded `matches!` in `gc.rs`: a new type has to pick a side. Observations
  still go as orphans and disconnected components. The score cut now removes
  only Entities and isolated Resources. The "most evictable kind" wording in the
  `lambo_derive` `concept_type` schema description and in the type docs is gone
  (`docs(wire)`; no golden carries the text). Every gc.rs test that used an
  Observation as its score-cut victim now uses an Entity or an isolated Resource,
  and the cap-ordering tests put the Entity at the lower id so an id-ordered
  cap would pick the wrong one (before, the weakest victim was also the lowest
  id, so those tests could not tell ratio order from id order).
* **The recency window is 365 days** (`feat(gc)!`). `gc_headroom`'s
  lower-bound claim still holds (span-relative `1 − x/span` stays at or below
  GC's `1 − x/window` whenever the session is no older than the window; a wider
  window only widens it), and it is conservative because it still measures the
  exempt types.

Review findings, each its own commit: a second clock re-anchor in one process
is now persisted (a newer mark carrying the reset flag supplies `last_gc_at`
whether or not the older one is flagged; `last_gc_epoch` stays a strict max);
the writer-side reset flag is stripped in `Graph::snapshot` and
`Graph::from_snapshot`; the mangled test doc comments are back on their own
tests; `resources_spared_by_dependents` counts after the disconnected filter;
the never-swept catch-up is documented and pinned (decision 4); `lambo_stats`
reads GC stats once; and `SessionContext` precomputes each concept's derivation
count so a sweep is no longer O(concepts × interactions).

### Dry run, round 2 (`work29s.db`, fresh copy of the same snapshot; aggregates only)

Same store and `now = 2026-10-07T06:30Z`, default weights, copy provisioned with
this branch's binary, mark unset. 1,307 of 1,464 Resources have dependents (as
in round 1).

| clock | first sweep (capped = uncapped) | by type | Resources spared by the rule |
|---|---|---|---|
| +0, +7, +14, +30, +60, +180 d | 0 | — | 0 |
| +365 d | 126 | Resource 120, Entity 6 | 875 |
| +400 d | 126 | Resource 120, Entity 6 | 875 |

The cap (169) never binds. Successive capped sweeps, no writes, same clock or
+1 day each: 0 for all 12.

**Untouched end state** (uncapped sweeps at +365 d until nothing more goes):
**130 collected, 3.9% of the store — Resource 121, Entity 9; Observation, Logic,
Constraint 0.** 884 under-bar Resources are kept only by the dependents rule;
1,309 Resources with dependents remain. Blast radius total 1,165 → 1,184;
**concepts above Stage 3's blast-radius bar (> 5): 41 before, 41 after.**

**Synthetic access** ("newly" = collected with access but not without):

| scenario | accessed | first sweep | newly | end state | newly |
|---|---|---|---|---|---|
| no access | 0 | 0 | — | 130 | — |
| one access on the hub Entity, at its creation time | 1 | 0 | **0** | 130 | **0** |
| one access on the hub Entity, at `now` | 1 | 0 | **0** | 130 | **0** |
| replay k=1 | 2,115 | 0 | **0** | 69 | **0** |
| replay k=2 | 2,671 | 0 | **0** | 38 | **0** |
| replay k=4 | 3,065 | 0 | **0** | 11 | **0** |

**Sweep time** (`gc::run`, first sweep, median of 7, release build, the 3,370
concept snapshot): **67.5 ms before the `SessionContext` precompute, 9.0 ms
after** (min 65.6 → 8.5). Scoring every concept alone: 95.1 ms → 1.7 ms, with
an identical checksum, and every collection count above is identical before and
after (the per-concept scan is kept as a test oracle). What remains is the
`O(edges)` passes and the per-concept degree sort. The sweep runs under the
write lock; a 2.7k-concept store was the worst case measured here.

## Risks a reviewer should weigh

- **Observations are never collected by the score cut.** With the exemption an
  Observation leaves the store only as an orphan or a disconnected component, so
  a store of well-connected Observations grows until something else prunes it.
  By operator decision, and the rig's whole at-risk set is 3.9% of the store.

- **Resources and Entities age out — now only isolated ones.** Before the
  operator's dependents rule, 995 of 1,464 Resources were below their bar once
  older than the window. With it, and with Observation exempt, the untouched end
  state at a 365-day window loses 130 concepts (Resource 121, Entity 9); 884
  under-bar Resources are kept because something depends on them, and the count
  of concepts above Stage 3's blast-radius bar is unchanged (41).
  `MIN_CONCEPT_SCORE` and every promotion threshold are as they were.
- **The cap rarely binds now.** The rig's first sweep at +365 d takes 126
  against a cap of 169; the warning and `lambo_stats`' `gc` block (`deferred`,
  `cap_bound`) are for a backlog that a long pause or a scoring change builds.
- **Lost deferred bumps.** Pending survivor bumps live in the daemon, not the
  store. A restart mid-drain loses the rest of that sweep's bumps (as before
  #29); the watermark already counts the drained part, so nothing double-counts.
  The drain order used to be id-ascending, so the lost tail was always the
  same high ids; it is now rotated per sweep by a mix of the sweep's starting
  epoch (`gc::survivor_drain_order`), which spreads the loss evenly. Persisting
  the pending set was not done (a new column for a bounded, now unbiased
  delay).
- **Postgres/Cockroach SQL untested live.** The pg-family upsert, seed and load
  are compile- and string-parity-covered offline only (no live database here).

## Interface points with #30

- GC's measure is `epoch − last_gc_epoch`. A write that does **not** advance
  the epoch is already invisible to it and needs no exemption; #30 records
  accesses without advancing the epoch, so it must **not** call
  `Graph::exempt_from_gc_measure` (that would cancel real session writes out
  of `gc_interval` and the idle floor and suppress sweeps). The seam is only
  for writes that did advance the epoch, with `n` equal to the bumps they
  appended, under the same write guard (GC's own survivor drains). It now
  clamps the watermark to the epoch and debug-asserts an overshoot.
- GC's eviction recency reads `last_accessed`; #30's writes protect recalled
  concepts directly. GC no longer switches to the full composite when
  `access_count` becomes live: each concept scores the live-dimension score
  plus its own frequency term (`score::score_live_plus_frequency`), so an
  unread concept's GC score is the same however much else is read. Recall
  ranking, the daemon's score table and canonization still use `score()`;
  any cliff there is #30's.
