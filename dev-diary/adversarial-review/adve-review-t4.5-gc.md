# Adversarial Review: T4.5 — GC ★ (canonization's food)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT                                             ║
║  Opened / Closed: 2026-08-12                                     ║
║  RECORD RECONSTRUCTED POST-HOC (XP-2 remediation, 2026-08-12)    ║
╚══════════════════════════════════════════════════════════════════╝
```

> **Reconstruction notice.** No review record was committed at the time. Rebuilt
> on 2026-08-12 from the PHASE-4 status line, the commit history, and the code,
> as remediation for **XP-2** of `adve-review-p4-daemon-opus.md`. Sections headed
> **Not recoverable** say what is lost rather than inventing review prose.

**Task:** T4.5 — periodic GC, spec §9 steps 1–7. Blocks P6: `gc_survived` is
canonization Stage 1's input, which is why GC cannot be cut.
**Scope:** `src/daemon/gc.rs` (new, 856 lines), `src/graph/graph.rs` (+70 —
`bump_gc_survived`), `src/daemon/mod.rs` (+1)
**Implementing commit:** `74ad3c1` — *"P4 T4.5: GC (7-step periodic, gc_survived,
epoch, budget record)"*
**Merged:** `c8f64f6` (`task/p4-t4.5-gc` → `phase/p4-daemon`)
**Status line** (PHASE-4-daemon.md, section *"T4.5 — GC"*): *"done (2026-08-12,
reviewed ACCEPT; merged c8f64f6)"* — no remediation round claimed

## What the review had in front of it

Reconstructed from the shipped module (9 tests at merge). `gc.rs`'s header
carries a step-by-step spec→code mapping, which is the surviving evidence of what
the round checked:

1. **Edge cleanup** — `weight < min_edge_weight` (0.5) and last reinforcement
   older than `gc_edge_ttl` (1h). The TTL anchor is `last_reinforced`. *All seven
   edge types were eligible*, on the argument that the spec names none.
2. **Concept cleanup** — orphans + sub-threshold, excluding
   Venerable/Canonical/root-goal. `MIN_CONCEPT_SCORE = 0.3`,
   `ScoringWeights::default()`.
3. **Disconnected-component cleanup** — cycle-safe BFS from the temporal chain
   over the undirected graph, honoring the G6 binding note; protected classes
   exempt; interactions append-only and never collected.
4. **Index maintenance** — `sync_index(&outcome, &mut index)` as the hook,
   because the inverted index is owner-side by the P3 contract and
   `run(&mut Graph, …)` structurally cannot reach it.
5. **`gc_survived += 1` on all survivors** — via `Graph::bump_gc_survived`
   (saturating `i32`), each bump emitting an `UpsertNode` so the durable store
   mirrors the counter.
6. **Canonical budget** — GC *records* the count and the over-budget flag; T6.4
   demotes. GC never demotes.
7. **`MutationEpoch`** — bumped by GC's own mutations; `epoch_before`/
   `epoch_after` prove it.

`max_concept_nodes` advisory-only: warn, never evict (spec §9's "capacity is
elastic").

The header also records a careful **fixture note**: `session-drift`'s "isolated
widget"/"isolated sibling" pair is named GC step-3 food by the generator, but the
generated JSON also carries their `Derives` provenance edges, which *do* connect
them to the temporal chain in the loaded graph. The fixture test therefore drops
those two edges first — flagged in the docs as a TEST-ONLY state reconstruction,
not a step-1 behavior, since both edges sit at weight 0.9 and step 1 never
touches them. That distinction being written down is good practice and survives.

## Verified clean (re-verified 2026-08-12 against the shipped code)

- Steps run in spec order, and step 2 scores against **post-step-1** state.
- All outcome vectors are id-ascending, so a run is deterministic.
- Protected classes are computed once per run (statuses cannot change mid-run)
  and are honored by both step 2 and step 3.
- GC never demotes; the budget is recorded only.
- The later tier review independently verified GC's **mutation-emission parity**
  and found it perfect: every graph mutation GC makes emits (`DeleteEdge`,
  cascaded `DeleteNode`, `UpsertNode` per survivor bump,
  `CanonizationTransition` for auto-Venerable) and both MemoryStore and
  SqliteStore apply them. Store divergence: nil.
- Epoch coherence: GC's mutations and epoch bumps happen under one write guard,
  and `last_gc_epoch` rebases to `epoch_after`, so GC's own mutations cannot
  re-trigger it.

## Not recoverable

- **Any reviewer notes**, including whether `MIN_CONCEPT_SCORE = 0.3` was ever
  checked against an actual score distribution. Nothing in-repo records a
  calibration argument for it; the const's doc comment at merge said only that
  v0.6.0's value is not in-repo and 0.3 was a v0.1 decision. One commit on the
  task branch, no remediation round claimed, nothing to diff.
- **Gate numbers at close.** Not captured in-repo.

## Findings reopened by the later tier review

This is the task the tier review hit hardest, including its lead P1:

| ID | Issue |
|---|---|
| ALGO-1 (P1) | `MIN_CONCEPT_SCORE = 0.3` mass-collects a healthy session: 15 of `session-rest-api`'s 22 concepts on the first sweep, including `auth middleware` (which spec §13 step 1 names), leaving 6 non-Canonical peers where canonization Stage 1 requires 20. GC starved the pipeline it exists to feed. `frequency` is identically 0 until P5 lands, so a fifth of the composite was dead weight in the cut. |
| CONC-1 (P1) | The step-2 rescore ran `incident_edges` per concept inside the write guard, and `incident_edges` was a full edge scan — a measured 272ms write guard at 4k concepts |
| ALGO-4 | The cut hardcoded `ScoringWeights::default()`, so eviction and recall could rank with two different score functions and `GcParams` could not express the session's weights |
| ALGO-9 | Step 1 ignored the §5 decay table: only `CoOccurrence`/`Semantic` decay, and `record_action` writes `Causal`/`Dependency` at exactly `MIN_EDGE_WEIGHT` against a strict `<` — a zero-margin §5.7 violation waiting on any weight tweak |
| ALGO-11 | `ConceptType::eviction_resistance` had zero call sites: Constraints (1.5) and Observations (0.7) faced the identical flat cut |
| CONC-6 (+XP-10) | One `UpsertNode` clone per survivor per run inside the write guard — up to 10k mutations / 20 flush batches per sweep |
| XP-5 | `GcOutcome` was dropped except `epoch_after`: T6.4's budget signal unreachable, the advisory warning unobservable, `sync_index` uncallable by construction |

**Why the suite was blind to ALGO-1:** `gc.rs`'s tests loaded only
`session-drift` (9 concepts, all scoring 0.57–0.71). No test ran `gc::run`
against `session-rest-api` — the demo session. A per-task record with a stated
scope would have made "GC has never been run against the demo fixture" a visible
gap; without one, a green 9-test module read as covered.
