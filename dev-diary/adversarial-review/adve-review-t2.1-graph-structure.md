# Adversarial Review: T2.1 — Graph Structure & Invariants

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after remediation (all findings      ║
║               fixed or documented)                       ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.1 — Graph structure & invariants
**Scope:** `src/graph/mod.rs`, `src/graph/graph.rs`, `src/resolve.rs` (pre-existing
no-default-features gate fix)
**Remediation commits:** `7f3b6a3` (code), + doc close (this file, PHASE-2 handoff log)
**Gate at close:** `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo clippy --no-default-features --all-targets -- -D warnings`; full suite
**119** lib tests pass (was 113 pre-review; +6 remediation tests), 3 consecutive runs.

## Close record — finding dispositions

| # | Finding | Disposition | Evidence |
|---|---------|-------------|----------|
| M1 | `dfs_cycle` misses `Hierarchical` | **Fixed** | `dfs_cycle` now walks Causal + Dependency + Hierarchical; `hierarchical_cycle_is_detected_by_assert_invariants` test. Write-time rejection stays per spec §5.7 (T2.4); the safety net is broader. |
| M2 | Log ordering vs §2.4 phased contract | **Fixed (option a)** | `drain_log` and module docs now state: batches are **chronological**; §2.4 grouping holds *within* a logical write only; adapters replay in order, never re-sort. `mutation_log_is_chronological_across_interleaved_writes` locks the create→delete→create sequence. |
| S1 | Incoming weight discarded on reinforce | **Documented** | `upsert_edge` doc: duplicate write is a fixed-bump reinforcement (v0.6.0 §5.4), not a re-weight; delete first to re-weight. Semantics kept. |
| S2 | `remove_node` on interaction corrupts chain silently | **Fixed** | `remove_node` rejects `Interaction` nodes (append-only, spec §9 compaction cut) with a typed error; `remove_node_rejects_interactions` test. |
| S3 | Incident-edge scan is O(\|E\|) | **Fixed** | Incident edges now collected from the `out`/`incoming` adjacency index via `edge_keys` reverse lookup (O(degree)), with self-loop dedup. |
| S4 | `snapshot()` sorts interactions by UUID, not chain order | **Fixed** | Interactions keep temporal-chain order (sort removed); concepts/edges still id-sorted. Docstring now matches behavior. |
| S5 | Round-trip test only checks snapshot equality | **Fixed** | New `snapshot_roundtrip_preserves_structure` compares structural queries across the round-trip. **It caught a real bug**: `out_neighbors`/`in_neighbors`/`incident_edges` returned HashSet iteration order — flaky under per-process hash seeds. All neighbor accessors now return deterministic id-ascending order. |
| I1 | CanonizationStatus naming vs spec §9 stages | **Noted** | Handoff log note for P6 (map stage numbers to enum variants; naming frozen by T1.1). |
| I2 | `reinforcements` starts at 1 vs DDL default 0 | **Noted** | Handoff log documents the "creation = first write" convention; adapters (T3.2) must match graph core. |
| I3 | Chain-walk cycle check O(n²) | **Fixed** | `HashSet` visited set instead of `Vec::contains`. |
| I4 | No Send/Sync proof for `Arc<RwLock<Graph>>` owner | **Fixed** | Compile-time `const _` assertion in tests. |

**Also fixed (not a review finding, required by the close gate):** `cargo clippy
--no-default-features` failed on unused test imports in `src/resolve.rs` (introduced
post-T1.4-close). Imports moved into the feature-gated test body.

## Original findings

Preserved below verbatim. All dispositions recorded above.

```yaml
reviewer:    adversarial-review-agent
date:        2026-08-11
task:        T2.1
owns:        src/graph/mod.rs, src/graph/graph.rs
status:      CLOSED (2026-08-11) — all findings remediated, see close record
verdict:     ACCEPT after remediation (was CONDITIONAL ACCEPT: 2 must-fix, 5 should-fix, 4 informational)
```

---

## Scope

This review covers `src/graph/mod.rs` + `src/graph/graph.rs` (~1,500 LOC)
against the frozen spec (`lambo-hackathon-spec-v0.1.md`), the P2 phase doc,
the T2.1 handoff log, the T1.4 adversarial review findings, and both committed
fixture snapshots.

---

## Findings

### MUST-FIX

#### M1. `dfs_cycle` does not cover `Hierarchical` edges

**Spec §5.7:** "No cycles in `Causal` or `Dependency`, enforced at write time
by BFS."

The spec only names `Causal` and `Dependency`, and `dfs_cycle` correctly limits
itself to those two edge types. However, `Hierarchical` edges (`parent_of`)
form a DAG by definition — a cycle in `Hierarchical` is semantically
nonsensical (A is parent of B is parent of A). The spec's §5 table shows
`Hierarchical` connects `Concept -> Concept` with no decay, identical to
`Causal`/`Dependency`.

**The risk:** T2.3 (`derive()`) will create `Hierarchical` edges from
`parent_of` fields. If a cycle is introduced through `Hierarchical` edges,
`assert_invariants()` will not detect it. Downstream tasks (P5 recall BFS, P6
canonization structural queries) that traverse `Hierarchical` edges will
infinite-loop or produce wrong results.

**Location:** [`graph.rs:936-961`](file:///home/nryn/work/lambo/src/graph/graph.rs#L936-L961)

```rust
// Current: only Causal + Dependency
let causal = self.out_neighbors_typed(node, EdgeType::Causal);
let dependency = self.out_neighbors_typed(node, EdgeType::Dependency);
for tgt in causal.into_iter().chain(dependency) {
```

**Fix:** Add `Hierarchical` to the cycle check. The spec's silence on
`Hierarchical` cycles is an omission, not a permission — the v0.6.0 design
this is narrowed from treats all directed concept-concept edges as DAG
constraints. File a spec errata note if desired, but the code should be
defensive.

---

#### M2. Mutation log ordering does NOT match §2.4 flush contract

**Spec §2.4:** "Apply node upserts, then edge upserts, then deletions, then
canonization transitions."

The mutation log is append-only in chronological write order. It does **not**
reorder mutations into the §2.4 phase ordering (nodes -> edges -> deletions ->
transitions). The `drain_log_clears_and_orders_writes` test at line 1397
verifies that individual operations happen to emit in the right order *within
each logical write*, but this is **coincidental** — it holds because the test
performs operations in a specific sequence.

**The real contract gap:** Consider this sequence of writes within one flush
interval:

1. `insert_interaction(i1)` -> emits `UpsertNode(i1)`
2. `insert_concept(c1, i1)` -> emits `UpsertNode(c1)`, `UpsertEdge(derives)`
3. `remove_node(c1)` -> emits `DeleteEdge(derives)`, `DeleteNode(c1)`
4. `insert_concept(c2, i1)` -> emits `UpsertNode(c2)`, `UpsertEdge(derives2)`

The drained log is:
```
UpsertNode(i1), UpsertNode(c1), UpsertEdge(derives),
DeleteEdge(derives), DeleteNode(c1),
UpsertNode(c2), UpsertEdge(derives2)
```

An `UpsertNode` appears **after** `DeleteNode` in the batch. If the flush task
(T3.4) blindly sends this to the store in order, it works. But if T3.4
reorders to match §2.4's "upserts then deletions" contract, it will try to
delete `c1` before `c2` is created — which is fine — but the interleaving
violates the stated ordering guarantee.

**Impact:** T3.4 (flush task) and every store adapter must decide: replay
in-order (safe, but violates the §2.4 phased grouping), or reorder (matches
§2.4 but may break interleaved create/delete sequences). The current code
does not resolve this ambiguity.

**Fix:** Either:
- (a) Document that `drain_log()` returns chronological order and that §2.4's
  phase ordering applies *within* a single logical write, not across the batch.
  Update the docstring on `drain_log` and the module doc.
- (b) Have `drain_log()` sort mutations into §2.4 order (upserts first, then
  deletes, then transitions). This requires careful handling of
  create-then-delete-then-create sequences.

Option (a) is recommended — it's what the code actually does and it's safe for
any store that replays in-order.

---

### SHOULD-FIX

#### S1. Reinforcement ignores the incoming edge's weight entirely

**Location:** [`graph.rs:879-886`](file:///home/nryn/work/lambo/src/graph/graph.rs#L879-L886)

When a duplicate natural-key edge is written, the existing edge's weight is
bumped by `REINFORCE_BUMP` (a fixed +1.0), completely ignoring the weight on
the incoming edge. The incoming edge's `weight` field is discarded.

**Spec §5:** "Weight dynamics: v0.6.0 §5.4 unchanged." The v0.6.0 §5.4
semantics specify that reinforcement is a fixed bump, so the current behavior
is arguably correct. However, the `weight` parameter on the incoming edge
is silently swallowed — callers may expect it to matter.

**Risk:** T2.3 (`derive()`) and T2.4 (`record_action()`) will pass
`CoOccurrence` and `Causal`/`Dependency` edges with specific weights. Those
weights will be ignored on reinforcement. If a caller ever sets a weight
expecting it to replace the existing weight (e.g., a daemon score update),
the silent discard will cause subtle scoring bugs.

**Fix:** Document this explicitly in the `upsert_edge` docstring. Add a note
that reinforcement always bumps by `REINFORCE_BUMP` regardless of the incoming
edge's weight. Alternatively, consider using `max(existing + REINFORCE_BUMP,
incoming.weight)` to at least not lose information.

---

#### S2. `remove_node` on an interaction corrupts the temporal chain silently

**Location:** [`graph.rs:449`](file:///home/nryn/work/lambo/src/graph/graph.rs#L449)

```rust
self.temporal_chain.retain(|&x| x != id);
```

Removing a mid-chain interaction leaves a gap: the next interaction's
`previous_id` still points to the removed node. After removal,
`assert_invariants()` will fail because the chain's `previous_id` links are
broken. But the removal itself succeeds — the invariant violation is detected
lazily, not at write time.

**Spec §5.7:** "Every non-first interaction has exactly one Temporal
predecessor." Removing that predecessor without updating successors violates
this by construction.

**Risk:** GC (T4.5) must remove nodes. If it ever removes an interaction
(unlikely but not forbidden by the spec), the graph silently corrupts.

**Fix:** Either:
- Reject `remove_node` on `Interaction` nodes (they're append-only by design).
- Or fix the chain linkage (predecessor's successor inherits removed node's
  predecessor).

The first option is simpler and correct per spec — interactions are never
removed in v0.1.

---

#### S3. Adjacency cleanup in `remove_node` is O(|E|) full-edge scan

**Location:** [`graph.rs:439-444`](file:///home/nryn/work/lambo/src/graph/graph.rs#L439-L444)

```rust
let incident: Vec<NodeId> = self
    .edges
    .iter()
    .filter(|(_, e)| e.source == id || e.target == id)
    .map(|(eid, _)| *eid)
    .collect();
```

This scans every edge in the graph to find incident edges, despite the
adjacency index (`out`/`incoming`) already having this information.

**Impact:** For the expected graph sizes (~1,000 nodes, ~5,000 edges), this is
fine. For future scalability, it's an unnecessary O(|E|) scan when the
adjacency maps provide O(degree) lookup.

**Fix:** Collect incident edge IDs from `self.out.get(&id)` and
`self.incoming.get(&id)` via the `edge_keys` reverse lookup:
```rust
let mut incident = Vec::new();
if let Some(by_type) = self.out.get(&id) {
    for (ty, targets) in by_type {
        for &tgt in targets {
            if let Some(&eid) = self.edge_keys.get(&(id, tgt, *ty)) {
                incident.push(eid);
            }
        }
    }
}
// ... similarly for incoming
```

---

#### S4. `snapshot()` sorts interactions by UUID, not chain order

**Location:** [`graph.rs:249`](file:///home/nryn/work/lambo/src/graph/graph.rs#L249)

```rust
interactions.sort_by(|a, b| a.id.0.cmp(&b.id.0));
```

The docstring says "interactions in temporal chain order" but the code sorts by
UUID. These happen to match for the committed fixtures (which use
lexicographically-ordered UUIDs), but for production data with random v4 UUIDs,
the orders will diverge.

**Impact:** `from_snapshot` rebuilds the chain from `previous_id` links, so
loading still works. But the snapshot's `interactions` array is documented as
being in chain order — any consumer relying on that (T5.3 recall context
format, T8.x MCP responses) will get wrong ordering with real UUIDs.

**Fix:** Remove the sort — the `filter_map` over `temporal_chain` already
produces chain order. The sort destroys it:
```rust
// interactions already collected in chain order from temporal_chain iter
// concepts and edges still sort by id for determinism
concepts.sort_by(|a, b| a.id.0.cmp(&b.id.0));
edges.sort_by(|a, b| a.id.0.cmp(&b.id.0));
synonyms.sort_by(|a, b| a.source_key.cmp(&b.source_key));
```

---

#### S5. No `Eq`/`PartialEq` on `Graph` — snapshot round-trip test is weaker than it appears

The `fixture_rest_api_loads_and_passes_invariants` test asserts:
```rust
assert_eq!(g.snapshot(), snap2);
```

This tests snapshot equality, not graph equality. Internal state like the
adjacency index shape, the `edge_keys` map, and the mutation log are not
compared. A bug in `record_edge` that populates `edge_keys` incorrectly but
still produces correct snapshots would be invisible.

**Fix:** Add a dedicated test that mutates a graph, snapshots it, loads the
snapshot into a new graph, and then verifies that structural queries
(`out_neighbors_typed`, `in_neighbors_typed`, `edge_between`) return identical
results — not just that the snapshot serializes the same way.

---

### INFORMATIONAL

#### I1. `CanonizationStatus` enum misaligns with spec §9 naming

The spec's §9 stages are: `Stage1 -> Stage2 -> Stage3`. The code uses:
`None -> Candidate -> Venerable -> Canonical`. The P2 phase doc's T2.1 handoff
log refers to `apply_canonization_transition(from, to)`, but the code takes a
`CanonizationEvent` whose `from_status`/`to_status` use the code enum, not the
spec's stage numbers.

This is not a bug (the T1.1 contracts froze this naming), but downstream tasks
(P6) must map spec stage numbers to code enum variants. Note for P6 agents.

---

#### I2. Edge `reinforcements` initializes to 1, not 0

**Location:** Test helpers and `insert_interaction`/`insert_concept` all set
`reinforcements: 1` on initial creation.

**Spec §4:** `reinforcements INT NOT NULL DEFAULT 0`.

The spec's SQL DDL defaults to 0, but the code starts at 1 (treating creation
as the first reinforcement). This is a valid interpretation ("the edge was
written once"), but it differs from the DDL. Store adapters (T3.2) must match
whichever convention the graph core uses.

**Note:** The T2.1 handoff log does not mention this decision. Add it.

---

#### I3. `from_snapshot` cycle detection in temporal chain is O(n^2)

**Location:** [`graph.rs:163`](file:///home/nryn/work/lambo/src/graph/graph.rs#L163)

```rust
if chain.contains(&next) {
```

`Vec::contains` is O(n). Called inside a loop walking the chain, this is
O(n^2). For the expected chain lengths (<1,000 interactions per session), this
is negligible. For adversarial inputs with very long chains, it could be slow.

**Fix (optional):** Use a `HashSet` for visited nodes instead of
`Vec::contains`.

---

#### I4. No test for concurrent access patterns

T2.1's scope explicitly excludes the `Arc<RwLock<Graph>>` wrapper ("Graph owns
no lock"). However, there is no test that verifies `Graph` is `Send + Sync`,
which is required for it to be wrapped in `Arc<RwLock<>>`.

**Fix (optional):** Add a compile-time assertion:
```rust
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assertions() { assert_send::<Graph>(); assert_sync::<Graph>(); }
};
```

---

## Test Coverage Assessment

### Covered well:
- Temporal chain construction and rejection of invalid positions
- Re-upsert idempotency for interactions and concepts
- Derives edge auto-creation and reinforcement
- Natural-key dedup with reinforcement mechanics
- Weight normalization (NaN, Inf, negative)
- Endpoint and session validation on edges
- Node removal with incident edge cleanup
- Cycle detection in `assert_invariants`
- Epoch mechanics (bumps on write, not on read/drain)
- Mutation log ordering within a single logical write
- Fixture snapshot round-trip (both snapshots)
- Canonization transition application

### Coverage gaps:
| Gap | Severity | Notes |
|---|---|---|
| No `Hierarchical` cycle detection test | High | Only `Dependency` tested (M1) |
| No cross-batch mutation ordering test | Medium | See M2 |
| No test for removing an interaction node | Medium | See S2 |
| No test for self-loop edges `(A, A, T)` | Low | `upsert_edge` would accept it |
| No test for `edge_between` after reinforcement | Low | Covered implicitly |
| No adversarial snapshot with >100 nodes | Low | Perf only |
| No test for `out_neighbors` dedup behavior | Low | Returns `HashSet` |
| Missing `Causal` edges in any fixture | Medium | Only in unit tests |
| Missing `Hierarchical` edges in fixtures | Medium | Only in weight normalization test |

---

## Downstream Impact

| Task | Dependency on T2.1 | Risk from findings |
|---|---|---|
| T2.3 `derive()` | Calls `insert_concept`, `upsert_edge` | S1 (weight silently dropped) |
| T2.4 `record_action()` | Calls `upsert_edge`, adds cycle check | M1 (Hierarchical gap) |
| T3.4 Flush task | Calls `drain_log()` | M2 (ordering ambiguity) |
| T4.5 GC | Calls `remove_node` | S2 (interaction removal) |
| T5.3 Recall context | Reads `snapshot()` | S4 (interaction order) |
| P6 Canonization | Reads `canonization_events` | I1 (naming) |

---

## Verdict

**CONDITIONAL ACCEPT.** The graph core is well-structured, the invariant
checker is thorough, and the test suite covers the critical paths. The two
must-fix items (M1: Hierarchical cycle gap, M2: mutation log ordering
documentation) should be addressed before T2.3/T2.4 and T3.4 respectively
start building on top of this code. The should-fix items are real but bounded
risk — S4 (snapshot ordering) is the most likely to bite a downstream task.
