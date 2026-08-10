# P1 — Contracts & fixtures

```yaml
id:       P1
requires: [T0.1]
blocks:   P2, P3, P4, P5, P6, P7
parallel: partial   # T1.1 first, then T1.2 ‖ T1.3, then T1.4
```

**Goal:** freeze the shapes, then hand every downstream track a complete in-RAM store and
fixture graphs so nobody waits for anybody — least of all for CockroachDB.

**This phase is small on purpose and load-bearing on purpose.** Its real output is not
code — it is `MemoryStore` + `fixtures/` that let P2–P7 run six-wide.

---

### T1.1 — Core types & config ★
```yaml
requires:   T0.1
fixture-ok: n/a
owns:       src/types.rs, src/config.rs
status:     not-started
```
Everything two tracks must agree on, from the spec:

- Ids: `NodeId(Uuid)`, `SessionId`, `AgentId`. Store issues UUIDs (spec §5 — no arena).
- Nodes: `Interaction`, `Concept` with all spec §4 columns as fields (`gc_survived`,
  `canonization_status`, `blast_radius`, `access_count`, …).
- Enums: `ConceptType` (Entity, Logic, Constraint, Resource, Observation — with the v0.6.0
  eviction resistances and score multipliers as associated consts), `EdgeType` (the seven
  retained, spec §5, each knowing `decays() -> bool`), `CanonizationStatus`
  (None, Candidate, Venerable, Canonical), `MatchStrategy` (Canonical, Hybrid).
- `Edge` with weight, reinforcements, timestamps.
- `Mutation` + `MutationBatch` — the write-behind unit (spec §2.4): node upserts, edge
  upserts, deletions, canonization transitions, **ordered**.
- `GraphSnapshot` — what `load_session` returns.
- `DaemonEvent` — Conflict, Drift, Stale, HighRisk, Canonized (spec §6.1).
- `RecallQuery`, `RecallResult`, `Scored<T>`, `InteractionSpan { distinct, coverage }`.
- `CanonizationEvent`, `Reservation`, `StoreError`, `LamboError`.
- `Config` with every named default from the spec in one place:
  `backend_flush_interval=1.0s`, `backend_flush_max_batch=500`, `backend_flush_retries=3`,
  `backend_log_max=50_000`, `scoring weights 0.25/0.20/0.20/0.35`, `w_daemon/w_query`,
  `hot_list_max=1000`, `conflict_recency_window=30s`, `drift_threshold=5`,
  `gc_interval=10_000`, `max_canonical_nodes=1000`, `canonization_min_peer_count=20`,
  `canonization_edge_min_age=60s`, `canonization_eval_interval=60s`,
  `canonization_eval_batch_size=50`, `canonization_repromotion_cooldown=300s`,
  `semantic_match_threshold=0.85`, `max_cooccurrence_per_derive=10`, `top_k`, `max_tokens`,
  `traversal_depth=2`.

All serde-serializable (fixtures are JSON). **Frozen after this task** — README convention 3.

**Done when:** every type round-trips JSON in tests and `Config::default()` matches the spec
value-for-value (test asserts each).

---

### T1.2 — `GraphStore` trait + `MemoryStore` ★★
```yaml
requires:   T1.1
fixture-ok: n/a
owns:       src/store/mod.rs, src/store/memory.rs
status:     not-started
```
The spec §3.2 trait verbatim: `init_schema`, `capabilities`, `flush`, `load_session`,
`keyword_candidates`, `vector_candidates`, `blast_radius`, `interaction_span`,
`record_canonization`. Plus `Capabilities` bitflags (`VECTOR_SEARCH | HISTORY`).

`MemoryStore`: complete in-RAM implementation, no I/O, **including both structural queries
computed naively** (blast radius by 1-hop scan, interaction span by walking
`origin_interaction`). Capabilities: neither flag. This is what makes P4/P5/P6 fixture-ok —
it must be *correct*, not fast; the SQL adapters are checked against it in T3.6.

**Done when:** trait compiles under `async_trait`, `MemoryStore` passes a conformance test
module (`tests/store_conformance.rs`, written generically so T3.2/T3.3 reuse it verbatim),
and no SQL type appears in the trait's signatures.

---

### T1.3 — `Embedder` trait + fixture embedder
```yaml
requires:   T1.1
fixture-ok: n/a
owns:       src/embed/mod.rs, src/embed/fixture.rs
status:     not-started
```
`Embedder`: `async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>`, dim
reporting, plus a `FixtureEmbedder` producing deterministic 1024-dim vectors (seeded hash →
unit vector) so hybrid matching (T7.2) and recall are testable offline and reproducibly.
Two fixture texts must land within `semantic_match_threshold` of each other and a third
outside it — document which, T1.4 and T7.2 depend on those pairs.

**Done when:** determinism test passes (same text ⇒ same vector) and the
near/far pair contract is asserted in a test.

---

### T1.4 — Fixture graphs ★★
```yaml
requires:   T1.1, T1.2
fixture-ok: n/a
owns:       fixtures/
status:     not-started
```
**The task that unblocks the swarm.** Committed, hand-checked JSON, loadable into
`MemoryStore` by a `fixtures::load(name)` helper (helper lives in `src/store/memory.rs`
behind `#[cfg(any(test, feature = "fixtures"))]`):

- `fixtures/session-rest-api.json` — the spec §13 demo world in miniature: two agents,
  ~12 interactions on a temporal chain, `user schema` with inbound `Dependency` edges from
  concepts originating in ≥6 distinct interactions spanning ≥0.3 of temporal extent and
  hypothetical-removal orphan count > 5 — i.e. **a node that lawfully passes all three
  canonization stages**, plus one that passes Stage 2 but fails Stage 3, plus one recent
  write from agent-A inside the conflict window.
- `fixtures/session-drift.json` — a root goal, one on-path concept, one concept > 5 hops
  from any root goal node (drift trigger), one disconnected component (GC step 3 food).
- `fixtures/mutations-batch.json` — a valid ordered `MutationBatch` exercising every
  mutation kind (flush-task and adapter test input).
- `fixtures/recall-goldens.json` — for `session-rest-api`: query → expected candidate ids
  (phase 1), expected expanded set at depth 2 (phase 2). Scoring is asserted structurally
  (ordering constraints), not on float equality.
- `fixtures/canonicalization-cases.json` — text → expected canonical key table covering
  hyphens, camelCase, stopwords, stemming, token-sort, synonym hit ("register_user" →
  "create_user"), and the T1.3 semantic near-pair.

**Done when:** every fixture loads through `MemoryStore` without invariant violations
(spec §5.7 checked by a test), and P4/P5/P6 agents can each start with zero network access.

---

## Exit criteria

- [ ] Types + config frozen, round-trip tested, defaults spec-exact
- [ ] `GraphStore` + `MemoryStore` + reusable conformance suite
- [ ] `Embedder` + deterministic fixture embedder with documented near/far pairs
- [ ] Fixtures committed — announced in Handoff Log; go signal sent (P2–P7 unblocked)

---

## Handoff Log

> _Fill on completion. Name any type or default that differs from the spec and why._
