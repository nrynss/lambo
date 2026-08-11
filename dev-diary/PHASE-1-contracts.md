# P1 — Contracts & fixtures

```yaml
id:       P1
requires: [T0.1]
blocks:   P2, P3, P4, P5, P6, P7
parallel: partial   # T1.1 first, then T1.2 ‖ T1.3, then T1.4; T1.5 with T1.2/T1.3
```

**Goal:** freeze the shapes, then hand every downstream track a complete in-RAM store and
fixture graphs so nobody waits for anybody — least of all for CockroachDB. Also land
**Level B** packaging (feature gates + registries + `lambo.toml`) so P3/P7 adapters plug
in without bloating the default binary.

**This phase is small on purpose and load-bearing on purpose.** Its real output is not
code — it is `MemoryStore` + `fixtures/` that let P2–P7 run six-wide.

---

### T1.1 — Core types & config ★
```yaml
requires:   T0.1
fixture-ok: n/a
owns:       src/types/, src/config.rs
status:     done
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
status:     done
```
The spec §3.2 trait verbatim: `init_schema`, `capabilities`, `flush`, `load_session`,
`keyword_candidates`, `vector_candidates`, `blast_radius`, `interaction_span`,
`record_canonization`. Plus `Capabilities` bitflags (`VECTOR_SEARCH | HISTORY`).

`MemoryStore`: complete in-RAM implementation, no I/O, **including both structural queries
computed naively** (blast radius by 1-hop scan, interaction span by walking
`origin_interaction`). Capabilities: neither flag. This is what makes P4/P5/P6 fixture-ok —
it must be *correct*, not fast; the SQL adapters are checked against it in T3.6.

**Level B:** gated on Cargo feature `store-memory` (default-on). Registered in
`build_store` for `StoreKind::Memory`.

**Done when:** trait compiles under `async_trait`, `MemoryStore` passes a conformance test
module (`tests/store_conformance.rs`, written generically so T3.2/T3.3 reuse it verbatim),
and no SQL type appears in the trait's signatures.

---

### T1.3 — `Embedder` trait + fixture embedder
```yaml
requires:   T1.1
fixture-ok: n/a
owns:       src/embed/mod.rs, src/embed/fixture.rs
status:     done
feature:    embed-fixture
```
`Embedder`: `async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>`, dim
reporting, plus a `FixtureEmbedder` producing deterministic 1024-dim vectors (seeded hash →
unit vector) so hybrid matching (T7.2) and recall are testable offline and reproducibly.
Two fixture texts must land within `semantic_match_threshold` of each other and a third
outside it — document which, T1.4 and T7.2 depend on those pairs.

**Level B:** gated on `embed-fixture` (default-on). Registered in `build_embedder` for
`EmbedderKind::Fixture`.

**Done when:** determinism test passes (same text ⇒ same vector) and the
near/far pair contract is asserted in a test.

---

### T1.4 — Fixture graphs ★★
```yaml
requires:   T1.1, T1.2
fixture-ok: n/a
owns:       fixtures/
status:     done
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

### T1.5 — Level B packaging (features + registries + `lambo.toml`) ★
```yaml
requires:   T1.2, T1.3
fixture-ok: n/a
owns:       Cargo.toml [features], src/store/mod.rs (build_store), src/embed/mod.rs
            (build_embedder), src/config.rs (LamboFile), lambo.example.toml,
            dev-diary/notes/level-b-pluggability.md
status:     done
```
Sustainability model (spec §3.4):

1. Cargo features: `store-memory`, `store-cockroach`, `store-sqlite`, `embed-bge`,
   `embed-fixture`, `embed-bedrock`, `fixtures`, convenience `demo`.
2. Optional deps only behind features (`sqlx`, AWS SDK, `reqwest`).
3. Registries `build_store` / `build_embedder` — **fail closed** if kind not compiled or
   adapter not implemented.
4. `LamboFile` parses `lambo.toml`; env overlays file; discover path via `--config` /
   `LAMBO_CONFIG` / `./lambo.toml`.
5. Design note is the packaging contract for P3/P7/P8/P9.

**Done when:** default `cargo test` stays lean (no sqlx/AWS required); selecting an
uncompiled kind errors with a feature rebuild hint; `lambo.example.toml` parses in tests.

---

## Exit criteria

- [x] Types + config frozen, round-trip tested, defaults spec-exact
- [x] `GraphStore` + `MemoryStore` (+ memory unit tests; full `tests/store_conformance.rs` can land with T3.x)
- [x] `Embedder` + deterministic fixture embedder with documented near/far pairs
- [x] Fixtures committed — announced in Handoff Log; go signal sent (**P2–P7 unblocked**)
- [x] Level B packaging landed (T1.5): features, registries, `lambo.toml`

---

## Handoff Log

### T1.1 / T1.2 / T1.3 (2026-08-10)

- Types live in `src/types/mod.rs` (not a single `types.rs` file — same module path `lambo::types`).
- Config in `src/config.rs` with `Config::default()` matching spec defaults (asserted in tests).
- `GraphStore` + `Capabilities` + `MemoryStore` in `src/store/`. Memory has no VECTOR_SEARCH;
  `vector_candidates` returns `StoreError::Capability`. Structural queries implemented naively.
- `FixtureEmbedder`: 1024-dim unit vectors; near pair `("register user", "create account")`
  cosine ≥ 0.85; far `"quantum chromodynamics lattice gauge"` < 0.85.
- **Still open: T1.4 fixtures** — required to fully unblock the swarm.
- Layout note: phase doc listed `src/types.rs`; implementation uses `src/types/` directory.

> Name any type or default that differs from the spec and why: none intentional.

---

### T1.4 — Fixture graphs (2026-08-11) — DONE — swarm unblocked

- **What exists now:**
  - `fixtures/*.json` (5 files), generated deterministically by `scripts/gen-fixtures.py`
    (commit the generator + output; re-run `python3 scripts/gen-fixtures.py` to regenerate).
  - `src/fixtures.rs` (feature `fixtures`, default-on): `load_snapshot`, `load_store`
    (seeds a `MemoryStore` via new `MemoryStore::seed`), `load_mutation_batch`,
    `load_recall_goldens`, `load_canonicalization_cases`.
  - P2–P7 can now start with zero network access: `fixtures::load_store("session-rest-api")`.
- **Fixture semantics (verified by `src/fixtures.rs` tests):**
  - `session-rest-api`: user schema passes all three canonization stages (gc_survived 4;
    **21 non-Canonical peers (>= canonization_min_peer_count=20)**; interaction_span distinct 6,
    coverage ~0.455; blast_radius 8). `api layer` passes Stage 2 (distinct 3, coverage 0.545)
    but fails Stage 3 (computed blast_radius 1 <= 5; its exclusive dependent is only
    `api docs` because `caching layer` also receives from `load testing`).
    Caching layer = recent agent-a write near session end; use `fixtures::load_store_relative`
    to rebase so that write lands inside `conflict_recency_window` against `Utc::now`.
  - `session-drift`: root goal (Venerable), on-path chain <= 5 hops, `far budget concept`
    at 6 hops (drift trigger), and an isolated 2-node disconnected component (GC step 3 food).
  - **§5.7 legal:** both session graphs carry a Temporal chain (non-first interaction -> its
    predecessor) and a Derives edge (origin interaction -> concept) for EVERY concept;
    no duplicate (source,target,edge_type); no Causal/Dependency cycles; weights finite/>=0.
    Verified by `fixtures::tests::satisfies_spec_57_invariants`.
  - `mutations-batch`: all five mutation kinds in spec §2.4 order with SPEC-LEGAL endpoint
    types (Temporal I->I, Derives I->C, Dependency C->C); delete_node cascades its edges and
    delete_edge targets an untouched Temporal edge; canonization transition hits a survivor.
  - `canonicalization-cases`: canonical-key table for T6. **Convention** (must match T6):
    lowercase -> split `[-_ ]` + camelCase -> drop stopwords -> Porter stem
    (`rust-stemmers`, verified) -> sort -> join `" "`. Synonym lookup on the RAW normalized
    key BEFORE stemming: `register_user` -> `create_user` -> key `"creat user"`.
    **Graph concept keys are generated by the same convention via a probe-verified stem table
    in `gen-fixtures.py`** (e.g. `auth middlewar`, `error respons`, `profil user`, `creat
    user`), so session keys and the cases table cannot drift apart. Add new words by re-probing
    `rust-stemmers`, never by hand-guessing. Semantic near-pair A/B (`register user`/
    `create account`) have DISTINCT keys (`regist user` vs `account creat`) — normalization
    must NOT merge them; hybrid §7.1 step 6 does. If T6's canonicalize diverges, update the
    fixture + this note in the same change.
  - `recall-goldens`: for `session-rest-api`; `phase1_candidates` are EXACT and now TESTED
    against `MemoryStore::keyword_candidates` (query `create` does not collide because the
    orphan was renamed to `user join time`). `phase2_expanded` lists REQUIRED members
    (candidate + direct neighbors). `pagination` -> candidate {pagination}; `create` ->
    candidate {create user}.
- **Cross-path change (flagged per conventions):** `MemoryStore::blast_radius` now counts
  ONLY aged inbound `{Dependency, Causal, Hierarchical}` edges from a concept source. This
  is required because spec §5.7 mandates a `Derives` (interaction -> concept) edge on every
  concept, but the literal §4.1 blast SQL ("any other inbound edge") would then treat that
  mandatory provenance edge as "dependent on another source" and zero out blast radius — a
  spec-internal inconsistency. Interaction-sourced `Derives`/`Temporal` edges no longer un-orphan
  a concept. Locked by `store::memory::tests::blast_radius_ignores_provenance_derives_edges`.
  P6 readers that recompute blast radius should use the same semantics.
- **Review disposition (2026-08-11):** `adve-review-t14-fixtures.md` **CLOSED — ACCEPT**.
  All must-fix items closed: (§5.7) Temporal + Derives emitted + invariant test; (Stage 1)
  21 non-Canonical peers; (recall) orphan renamed + phase1-exact tested; (canon keys)
  generated in-script via stem table; (mutations) spec-legal edge endpoint types; (conflict)
  `load_store_relative` rebases onto wall-clock so the recency window is runnable.
  Spec §4.1 blast SQL errata applied so adapters match MemoryStore (structural concept edges
  only; provenance Derives/Temporal excluded).

### T1.5 — Level B pluggability (2026-08-11) — DONE

- Design of record: `dev-diary/notes/level-b-pluggability.md` (kept current).
- Spec errata: §3.3 tables, §3.4 packaging + dim/contract rules, §6.1–§6.3.
- Cargo features + registries + `LamboFile` / `lambo.example.toml`.
- **`src/resolve.rs`:** `resolve_backends`, store×embedder dim check (store-authoritative
  `vector_dimensions`), `EmbeddingContract` on `GraphSnapshot`.
- CLI: single construction site (`ResolvedBackends`); no hardwired 1024 in embedder factory.
- Downstream: T3.2 implements `vector_dimensions() -> Some(n)`; T7.2/T8.1 stamp/check
  contract on session attach + hybrid write.