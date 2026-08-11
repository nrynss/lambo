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
status:     not-started
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

> _Fill on completion._
