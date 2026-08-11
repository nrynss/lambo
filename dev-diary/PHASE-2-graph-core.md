# P2 — Graph core (write path)

```yaml
id:       P2
branch:   phase/p2-graph-core
requires: [P1]
blocks:   P8
parallel: high   # T2.1 first; then T2.2 ‖ T2.6 ‖ T2.7; T2.3/T2.4/T2.5 after T2.2
runs-parallel-with: P3, P4, P5, P6, P7
```

**Goal:** the in-RAM bipartite graph and the three write APIs — `derive()`,
`record_action()`, `demote()` — per spec §5 and §7. This is the RAM tier; nothing here
touches the network.

**Concurrency rule (spec §6.4), non-negotiable:** graph is `Arc<RwLock<Graph>>`
(`parking_lot`), and **the lock is never held across an `.await`**.

---

### T2.1 — Graph structure & invariants
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/mod.rs, src/graph/graph.rs
status:     done (main, 2026-08-11)
```
Node/edge storage (`HashMap<NodeId, Node>`, adjacency in/out by edge type), the temporal
chain, mutation log emission (every mutation appends to the ordered log the flush task
drains — T3.4's input), `MutationEpoch` counter, and the spec §5.7 invariants enforced at
write time:

- every non-first interaction has exactly one `Temporal` predecessor
- every concept has ≥ 1 `Derives` edge
- no duplicate `(source, target, edge_type)`
- weights ≥ 0 and finite; NaN/Inf clamped to 0.0

Weight dynamics per v0.6.0 §5.4 as summarized in spec §5: reinforcement bumps on duplicate
edge writes; **recall does not reinforce**.

**Done when:** fixture graphs load and an `assert_invariants()` debug check passes on every
fixture and after every mutation in tests.

---

### T2.2 — Canonicalization pipeline
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/canonical.rs
status:     not-started
```
Spec §7.1 steps 1–5 (step 6, hybrid/vector, is T7.2's — leave a seam: the pipeline returns
`Unmatched(canonical_key)` and the caller decides):

1. normalize — lowercase, split hyphens/underscores/camelCase, strip stopwords
2. stem — Porter via `rust-stemmers`
3. token-sort → canonical key
4. synonym resolution — **direct lookup only**, no transitivity
5. match against existing `canonical_key`

`declare_synonym()` lives here too.

**Done when:** every row of `fixtures/canonicalization-cases.json` passes.

---

### T2.3 — `derive()`
```yaml
requires:   T2.1, T2.2
fixture-ok: yes
owns:       src/graph/derive.rs
status:     not-started
```
Spec §7 exactly: per concept — canonicalize → within-call dedup → match-or-create →
`Derives` edge from current interaction → pairwise `CoOccurrence` capped at
`max_cooccurrence_per_derive=10` → `Hierarchical` from `parent_of` → mutation batch →
daemon notify (a channel send; daemon side is T4.x).

**Done when:** deriving the same concepts twice creates no duplicates, reinforces
`CoOccurrence`, and emits well-ordered mutations.

---

### T2.4 — `record_action()` + cycle check
```yaml
requires:   T2.1, T2.2
fixture-ok: yes
owns:       src/graph/action.rs
status:     not-started
```
`Resource` concept for the action; `Causal` edges to `produces`/`modifies`, `Dependency` to
`depends_on`; implicit node creation through the full canonicalization pipeline; **BFS cycle
check over `Causal`/`Dependency` after canonical resolution** — reject the write, not the
process.

**Done when:** a crafted A→B→A dependency is rejected with a typed error and the graph is
unchanged after rejection.

---

### T2.5 — `demote()`
```yaml
requires:   T2.1
fixture-ok: yes
owns:       src/graph/demote.rs
status:     not-started
```
Context-overflow chunks → `Observation` nodes; UAX #29 sentence segmentation
(`unicode-segmentation`); `chunk_group_id` recorded for sibling co-retrieval (T5.2 reads
it). No custom split fn (cut).

**Done when:** a multi-sentence chunk yields one Observation per sentence sharing a
`chunk_group_id`.

---

### T2.6 — Inverted index + BM25
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/index.rs
status:     not-started
```
In-memory inverted index over concept content, per-session `df`, BM25 scoring — recall
phase 1's keyword source (spec §8). Incremental: updated on node create/update/remove, not
rebuilt. Reuses T2.2's normalizer for tokenization (import, don't fork).

**Done when:** the phase-1 keyword expectations in `fixtures/recall-goldens.json` pass
against fixture graphs.

---

### T2.7 — Reservations
```yaml
requires:   T2.1
fixture-ok: yes
owns:       src/graph/reserve.rs
status:     not-started
```
Spec §11 soft locks: advisory, expiring, same-agent re-reservation extends, cross-agent
returns `AlreadyReserved`. Surfaced in recall output (T5.3 reads active reservations).
**Cut-order note:** this is 4th in the cut order — keep it isolated so cutting is one
module delete.

**Done when:** extend/deny/expire paths are unit tested with mocked time.

---

## Exit criteria

- [ ] All fixture graphs constructible via public write APIs alone (a test rebuilds
      `session-rest-api` from scratch)
- [ ] Invariants hold after every test
- [ ] Mutation log ordering verified (nodes before edges referencing them)
- [ ] No `.await` inside any lock scope (grep + review)

---

## Handoff Log

### T2.1 — Graph structure & invariants (done 2026-08-11, by main)

**What exists now:** `src/graph/mod.rs` + `src/graph/graph.rs` (`Graph`, ~1.5k LOC
with tests), re-exported as `lambo::Graph`. In-RAM bipartite graph with:

- `HashMap<NodeId, Node>` nodes; `HashMap<NodeId, Edge>` edges (id PK) + natural-key
  index `(source, target, edge_type) -> id` (schema UNIQUE); per-node out/in
  adjacency grouped by `EdgeType` for recall BFS / structural queries.
- Temporal chain (`Vec<NodeId>`) built by construction in `insert_interaction`;
  `MutationEpoch` (bumps per appended mutation); ordered write-behind
  `mutation_log` drained by `drain_log()` (T3.4's input).
- Write APIs: `insert_interaction` (auto `Temporal` edge), `insert_concept` (auto
  `Derives` edge from origin interaction — §5.7 enforced at write time),
  `upsert_edge` (natural-key dedup + reinforcement), `remove_node` (emits incident
  `DeleteEdge`s before `DeleteNode`), `remove_edge`,
  `apply_canonization_transition`, `declare_synonym`, reservations, root_goal,
  embedding contract.
- `from_snapshot` (validates every invariant, seeds without log entries) +
  `snapshot()` (deterministic order: interactions in chain order, concepts/edges by
  id, synonyms by source_key — **round-trip exact for both fixtures**).
- `assert_invariants()` collects ALL §5.7 violations into one error (session
  consistency, endpoints, natural-key uniqueness, finite weights, chain,
  Derives coverage, Causal/Dependency acyclicity).

**Decisions the next agent must not re-derive:**
- **Reinforcement constants are ours** (v0.6.0 §5.4 constants not in-repo):
  `REINFORCE_BUMP = 1.0`, `MAX_EDGE_WEIGHT = 10.0`. Duplicate natural-key write:
  weight bumps (capped), `reinforcements += 1`, `last_reinforced` = write time, id +
  `created_at` preserved. Recall never reinforces (read path never calls edge
  writes).
- **Temporal edge direction: source = newer, target = previous** (points back in
  time, matching `scripts/gen-fixtures.py`). The predecessor invariant is therefore
  an **out**-edge check, not an in-edge check.
- Structural edge defaults mirror fixtures: `Temporal` w=1.0, `Derives` w=0.9.
- NaN/±Inf edge weights clamp to 0.0; negatives rejected (`Invariant`).
- `Causal`/`Dependency` cycle **rejection** is T2.4's BFS; `upsert_edge` stores what
  it's given and `assert_invariants` detects cycles.
- Synonyms + reservations are RAM-local (no `Mutation` kind exists); they round-trip
  through `GraphSnapshot` only. Reservations storage is a `Vec` preserving order.
- `Graph` owns no lock — `Arc<RwLock<Graph>>` + "never hold across `.await`" is the
  T2.3+ `Memory` owner's job (spec §6.4).
- Chain construction rejects forks/cycles/missing covers; re-upserting an
  interaction must keep its chain position.

**Verification:** 22 new tests, all green; full suite 113 passed / 1 ignored
(live-calibration needs llama-server). Both fixture snapshots load with
`assert_invariants` passing and `snapshot()` round-tripping exactly.

### T2.1 adve-review remediation (2026-08-11, by main — commit 7f3b6a3)

Adversarial review (`dev-diary/adversarial-review/adve-review-t2.1-graph-structure.md`)
CLOSED as ACCEPT. All 11 findings remediated; full dispositions in the review file.
Additional decisions downstream agents must know:

- **`assert_invariants` treats `Hierarchical` as a DAG constraint** (M1) — the
  safety net is broader than spec §5.7's write-time contract (Causal/Dependency,
  enforced by T2.4's BFS). A Hierarchical cycle is now a reported violation.
- **`drain_log` batches are chronological, never re-sorted** (M2) — §2.4's phase
  grouping holds within a single logical write only. A node upsert may legally
  follow a `DeleteNode` in one batch (create→delete→create). T3.4 must replay
  in order.
- **`remove_node` rejects interactions** (S2) — interactions are append-only in
  v0.1 (spec §9 compaction is cut). GC (T4.5) may only remove concepts.
- **Neighbor accessors are deterministic** (S5 fallout): `out_neighbors`,
  `in_neighbors`, `*_typed`, `incident_edges` return id-ascending order. The S5
  round-trip test caught a real HashSet-iteration-order flake.
- **`reinforcements` starts at 1** (I2): creation counts as the first write.
  Store adapters (T3.2) must match this convention.
- **CanonizationStatus naming** (I1): spec §9 stage numbers map to
  None→Candidate→Venerable→Canonical (frozen by T1.1). P6 must map, not rename.

Gate at close: `cargo fmt --check`; clippy `-D warnings` (default + no-default);
119 lib tests × 3 consecutive runs.

### T2.6 — Inverted index + BM25 (done 2026-08-11, by T26Index)

**What exists now:** `src/graph/index.rs` (`InvertedIndex`, ~490 LOC incl. tests),
registered via `pub mod index;` in `src/graph/mod.rs` (only line touched there).
Pinned API as spec'd: `new`, `add(&Concept)` (idempotent per node id — re-add
atomically replaces that concept's postings; a concept never appears twice),
`remove(NodeId)`, `from_snapshot(&GraphSnapshot)` (bulk = repeated `add`, never a
separate rebuild path), `search(query, limit) -> Vec<Scored<NodeId>>`. BM25
`k1 = 1.2`, `b = 0.75` (consts `BM25_K1`/`BM25_B` in-code, spec-pinned). Owns no
lock, synchronous, pure; no error paths (API returns `()`/`Vec`, so no
`LamboError`/`StoreError` needed).

**Tokenization — SEAM USED (T2.2's `canonical.rs` absent in this worktree at build
time).** Private `fn tokenize` implements the pinned T2.2 contract locally:
lowercase -> split `[-_ ]` + camelCase (incl. acronym-run split: `APIKey` ->
`api`,`key`) -> drop stopwords `{the,a,an,for,of,at,in,to,on,and,or,is,are}` ->
Porter stem (rust-stemmers `Algorithm::English`). Duplicate tokens retained (tf
counts occurrences). Marked with the exact seam comment
`// T2.6 seam: replace with crate::graph::canonical::normalize_tokens at
integration`; Main swaps at integration and the tokenizer contract test
(`tokenize_matches_canonical_contract`) double-checks the swap is behavior-neutral.
Did NOT touch `canonical.rs` (it doesn't exist here) or `graph.rs`.

**Decisions the next agent must not re-derive:**
- **Search semantics:** OR over query terms (a concept matching any term is a
  candidate; per-term BM25 contributions sum). `idf = ln(1 + (N - df + 0.5)/(df +
  0.5))` (BM25+ smoothing) so every matching term contributes a strictly positive
  score — no zero-score padding, matching the goldens' "positive score only" note.
  `df` is per-session by construction: one index per session; `df` = number of
  indexed concepts containing the term. Tie-break by `NodeId` (inner `Uuid`) —
  `NodeId` itself derives no `Ord`; sort uses `.0`.
- **Length bookkeeping:** every `add` increments `total_docs`/`total_tokens`; a
  zero-token concept (stopword-only/empty content) is a document but can never
  match (no postings) — avgdl stays well-defined.
- **Interactions are not indexed** (concepts only, per spec §8 / task contract);
  `from_snapshot` iterates `snap.concepts` only.
- `remove` is a no-op for unknown ids (matches `HashMap::remove` semantics).

**Verification:** 9 new unit tests (`cargo test graph::` — 37 passed, 0 failed,
92 filtered out; default features so `fixtures` gate is active). Both golden cases
pass with EXACT id sets and order via `from_snapshot(load_snapshot("session-rest-api"))`
+ `search(query, top_k)`: "pagination" -> `[1008]`, "create" -> `[1002]` (asserted
as id vectors, not floats, per goldens note). Fixture tests gated
`#[cfg(feature = "fixtures")]`. No fmt/clippy/project-wide suites run (per
constraints).
