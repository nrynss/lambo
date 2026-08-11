# Adversarial Review: T2.1 — Graph Structure & Invariants

```yaml
reviewer:    adversarial-review-agent
date:        2026-08-11
task:        T2.1
owns:        src/graph/mod.rs, src/graph/graph.rs
status:      CLOSED — ACCEPT (reverification audited 2026-08-11)
verdict:     ACCEPT — all findings addressed; no open items
```

---

## Scope

This review covers [`src/graph/mod.rs`](file:///home/nryn/work/lambo/src/graph/mod.rs) +
[`src/graph/graph.rs`](file:///home/nryn/work/lambo/src/graph/graph.rs) (~1,760 LOC)
against the frozen spec (`lambo-hackathon-spec-v0.1.md`), the P2 phase doc,
the T2.1 handoff log, the T1.4 adversarial review findings, and both committed
fixture snapshots.

**Verification method:** All claims were cross-checked against the current
source by an independent verification subagent reading the actual line numbers.
Five original draft findings were **refuted** because the code had already been
patched. This review reflects the verified state.

---

## Verified — Already Addressed

The following items from the initial draft review were found to already be
fixed in the current codebase. Listed here for traceability:

| Original ID | Finding | Current State |
|---|---|---|
| ~~M1~~ | `dfs_cycle` missing `Hierarchical` | **Fixed** — [line 1021](file:///home/nryn/work/lambo/src/graph/graph.rs#L1021) now chains `Causal`, `Dependency`, and `Hierarchical` |
| ~~S2~~ | `remove_node` on interaction corrupts chain | **Fixed** — [line 454-459](file:///home/nryn/work/lambo/src/graph/graph.rs#L454-L459) rejects interaction removal with an explicit error |
| ~~S3~~ | `remove_node` incident edge scan is O(\|E\|) | **Fixed** — [line 463-486](file:///home/nryn/work/lambo/src/graph/graph.rs#L463-L486) now uses adjacency index with `HashSet` dedup |
| ~~S4~~ | `snapshot()` sorts interactions by UUID | **Fixed** — [line 251-253](file:///home/nryn/work/lambo/src/graph/graph.rs#L251-L253) interactions are kept in chain order; only concepts/edges/synonyms sorted by id |
| ~~I3~~ | `from_snapshot` chain cycle detection O(n^2) | **Fixed** — [line 161-165](file:///home/nryn/work/lambo/src/graph/graph.rs#L161-L165) uses `HashSet<NodeId>` for O(1) lookup |

---

## Remaining Findings — audited against the tree (2026-08-11)

**Audit note:** the initial reverification pass listed S1/Q1/I2/I4 and two
coverage gaps as open. Re-checking each against the current source (post
remediation commit `7f3b6a3`, on `main`) shows **all of them were already
addressed** — the reverifier's draft was carried forward without re-reading
the docstrings/tests/handoff log. Verdict stands: ACCEPT, nothing open.

### ~~S1~~ — Verified fixed: mutation log ordering contract documented

`drain_log()` docstring ([graph.rs:746-750](file:///home/nryn/work/lambo/src/graph/graph.rs#L746-L750)):

```rust
/// The batch is in **chronological** write order. §2.4's phase grouping
/// (nodes -> edges -> deletions -> transitions) holds within a single logical
/// write, not across the batch. Replay in order — never re-sort.
```

The module doc (`src/graph/mod.rs`, "Mutation log contract") states the full
contract — chronological order, adapters MUST replay in order and MUST NOT
re-sort — and the interleaved create→delete→create sequence is locked by the
`mutation_log_is_chronological_across_interleaved_writes` test.

### ~~Q1~~ — Verified fixed: reinforcement weight discard documented

`upsert_edge` docstring ([graph.rs:427-430](file:///home/nryn/work/lambo/src/graph/graph.rs#L427-L430)):
"On reinforcement the incoming edge's `weight` is **intentionally ignored** — a
duplicate write is a reinforcement (fixed bump, v0.6.0 §5.4), not a re-weight.
Callers that want a different weight must delete the edge first."

### ~~I2~~ — Verified fixed: `reinforcements = 1` convention documented

Handoff log (`dev-diary/PHASE-2-graph-core.md`, remediation entry): "**`reinforcements`
starts at 1** (I2): creation counts as the first write. Store adapters (T3.2) must
match this convention."

### ~~I4~~ — Verified fixed: `Send + Sync` assertion present

`const _: () = { … assert_send::<Graph>(); assert_sync::<Graph>(); }` lives in the
graph tests module ([graph.rs:1755](file:///home/nryn/work/lambo/src/graph/graph.rs#L1755)),
compiled into the test binary.

### I1 — Informational (not a bug, frozen by T1.1)

`CanonizationStatus` (`None -> Candidate -> Venerable -> Canonical`) maps spec §9
stage numbers 1-3. P6 agents must map, not rename. Noted in the handoff log for P6.

---

## Test Coverage Assessment

### Covered well:
- Temporal chain construction and rejection of invalid positions
- Re-upsert idempotency for interactions and concepts
- Derives edge auto-creation and reinforcement
- Natural-key dedup with reinforcement mechanics
- Weight normalization (NaN, Inf, negative)
- Endpoint and session validation on edges
- Node removal with incident edge cleanup (using adjacency index)
- Interaction removal rejection (append-only guard)
- Cycle detection in `assert_invariants` (Causal + Dependency + Hierarchical)
- Epoch mechanics (bumps on write, not on read/drain)
- Mutation log ordering within a single logical write
- Fixture snapshot round-trip (both snapshots, chain-order interactions)
- Canonization transition application
- `from_snapshot` chain cycle detection (O(n) via HashSet)

### Remaining coverage gaps (post-remediation, audited):
| Gap | Severity | Notes |
|---|---|---|
| Missing `Causal` edges in fixture JSON | Medium | Covered by unit tests (`edge_weight_normalization`, `upsert_edge_validates_endpoints_and_session`); fixtures are frozen |
| Missing `Hierarchical` edges in fixture JSON | Medium | Covered by `hierarchical_cycle_is_detected_by_assert_invariants`; fixtures are frozen |

(The draft listed "no cross-batch mutation ordering test" and "no self-loop
test" as gaps — both landed with remediation: `mutation_log_is_chronological_across_interleaved_writes`
and `self_loop_structural_edge_is_a_cycle`.)

---

## Downstream Impact

| Task | Dependency on T2.1 | Remaining Risk |
|---|---|---|
| T2.3 `derive()` | Calls `insert_concept`, `upsert_edge` | None — reinforcement semantics documented (`upsert_edge` doc) |
| T2.4 `record_action()` | Calls `upsert_edge`, cycle check | None — Hierarchical covered |
| T3.4 Flush task | Calls `drain_log()` | None — chronological contract documented; replay in order, never re-sort |
| T4.5 GC | Calls `remove_node` | None — interaction guard in place |
| T5.3 Recall context | Reads `snapshot()` | None — chain order correct |
| T3.2 Store adapters | `reinforcements` default | Convention noted: creation = 1 (handoff log) |

---

## Verdict

**ACCEPT — CLOSED.** The graph core is well-structured, the invariant checker
is thorough, and every finding from the draft review is addressed in the
current code: the five code-structural findings verified fixed by the
reverification pass (M1, S2, S3, S4, I3), plus the doc/test-level items its
draft carried forward (S1, Q1, I2, I4) re-checked and confirmed present
(drain_log/module docs, `upsert_edge` doc, handoff log, Send+Sync assertion,
cross-batch and self-loop tests). Remaining items are informational only
(I1: P6 naming map; fixture edge-type coverage, which unit tests cover).
No blockers for downstream tasks.
