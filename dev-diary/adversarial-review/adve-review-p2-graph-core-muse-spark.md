```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — all findings dispositioned             ║
║  Branch: phase/p2-graph-core                             ║
║  Date:   2026-08-11                                       ║
║  Reviewer: muse-spark (adversarial, branch-level)        ║
║  Disposition: M1-M4 FIXED, S1 FIXED, S2/S4/S5 documented,║
║               S3 FIXED (new integration tests), S6 noted  ║
╚══════════════════════════════════════════════════════════╝
```

## Close record — dispositions

| # | Severity | Finding | Disposition | Evidence |
|---|----------|---------|-------------|----------|
| M1 | Major | `Graph` does not enforce `UNIQUE (session_id, canonical_key)` | **FIXED** — option (a): `insert_concept` rejects a same-key different-id non-Observation collision with a typed `Invariant` error; `assert_invariants` verifies key uniqueness (also catches violating snapshot loads); spec §4 errata (partial index) | `graph.rs` `insert_concept` check + `assert_invariants` key scan; tests `insert_concept_enforces_partial_canonical_key_uniqueness`, `assert_invariants_rejects_duplicate_canonical_keys_on_load` |
| M2 | Major | `demote()` intentionally violates UNIQUE (identical sentences -> same key) | **FIXED** — adopted option 1: UNIQUE is **partial** (`WHERE concept_type <> 'Observation'`); demote's context-overflow duplicates are legal; same rule in RAM (insert_concept/assert_invariants exempt Observations); spec §4 errata written | spec errata; Observation-exemption branches + test in M1's test |
| M3 | Major | `InvertedIndex` not wired to Graph; owner contract undocumented | **FIXED (contract)** — owner contract documented in `src/graph/mod.rs` ("InvertedIndex ownership"); sync sequence proven by `tests/p2_integration.rs::inverted_index_manual_sync_contract` (derive -> add -> search; remove_node -> remove -> excluded; demote -> add -> found). Wiring itself stays P3's `Memory` job, now with a tested contract | mod.rs doc; p2_integration test |
| M4 | Major | `derive()` not transactional — partial write on mid-call error | **FIXED** — validate-then-mutate: a read-only pre-pass validates the interaction AND hoists raw- + resolved-reflexive `parent_of` rejection (both ends canonicalize to the same key) before any write; the write loop is now infallible, so no partial batch is ever left in the graph or mutation log. Loop checks kept as unreachable defense | `derive.rs` pre-pass; module doc rewritten ("Validate-then-mutate") |
| S1 | Minor | `demote()` wall-clock `Utc::now()` vs logical time | **FIXED** — demote stamps `interaction.created_at` (deterministic, matches derive/record_action); `Utc` import dropped | `demote.rs` step 1 |
| S2 | Minor | Synonym raw lookup case/whitespace sensitivity | **DOCUMENTED** — canonical.rs `canonical_key` doc states keys are matched exactly on the trimmed input (case-sensitive, no folding) | `canonical.rs` doc |
| S3 | Minor | No branch-level integration test across all write APIs | **FIXED** — `tests/p2_integration.rs`: (1) derive+record_action+demote+reserve on one graph with invariants, chronological drain_log (nodes before referencing edges), snapshot round-trip incl. reservations; (2) index sync contract | two new integration tests, both green |
| S4 | Suggestion | CoOccurrence cap biases early concepts | **DOCUMENTED** — policy note in derive.rs (call-order pairs, prefix bias flagged for P5) | `derive.rs` step-5 comment |
| S5 | Suggestion | Reservations RAM-local, no durability | **DOCUMENTED** — durability contract stated in reserve.rs (persist only via GraphSnapshot; mutation log never carries them; crash between snapshot saves loses advisory locks) | `reserve.rs` module doc |
| S6 | Info | ParentOf + written_this_call interaction | No action — verified correct, already regression-tested | — |

**Verified at close:** `cargo test` = 193 lib + 2 main + 2 p2_integration + 1 rebuild, 0 failed;
`--no-default-features` 139+1; clippy `-D warnings` clean on both; `cargo fmt` applied.
Committed on `phase/p2-graph-core` (no merge to `main` per integrator instruction).

## Original findings

Preserved below verbatim; dispositions recorded above.

---

# Adversarial Review: P2 — Graph Core (branch `phase/p2-graph-core`) — muse-spark

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — all findings dispositioned             ║
║  Branch: phase/p2-graph-core (4433 insertions vs main)   ║
║  Spec:   lambo-hackathon-spec-v0.1.md (frozen)           ║
║  Phase:  dev-diary/PHASE-2-graph-core.md (P2, T2.1-T2.7) ║
║  Date:   2026-08-11                                       ║
║  Reviewer: muse-spark (adversarial, branch-level)        ║
║  Prior task reviews: T2.1-T2.7 closed ACCEPT (x2)         ║
╚══════════════════════════════════════════════════════════╝
```

## Scope & Grounding

Reviewed against frozen spec §§2,5,6.4,7,8,11 and P2 handoff log. Branch diff vs `main`:

- `src/graph/mod.rs`, `src/graph/graph.rs` (T2.1)
- `src/graph/canonical.rs` (T2.2), `src/graph/derive.rs` (T2.3), `src/graph/action.rs` (T2.4)
- `src/graph/demote.rs` (T2.5), `src/graph/index.rs` (T2.6), `src/graph/reserve.rs` (T2.7)
- `src/types/mod.rs` (`Concept.chunk_group_id`), `src/store/memory.rs` (1-line re-export), `tests/rebuild_session.rs`

Verification: read every new module + `graph.rs` invariants/mutation-log/edge path, checked diff `main...HEAD`, ran `cargo test --lib` (191 passed, 0 failed, 1 ignored) and `cargo clippy -D warnings` (clean). Prior per-task ACCEPTs are taken as given; this review audits **branch integration** and spec-level seams those reviews scoped out.

---

## Verdict Summary

P2 is structurally sound. Per-task logic is correct and tested. Branch-level gaps are integration contracts and spec tensions, not broken write paths. No blocker that invalidates the ACCEPTs — but **M1/M2 should be dispositioned before P3 flush lands**, and **M3/M4** need an owner.

| # | Severity | Title | Disposition |
|---|----------|-------|-------------|
| M1 | **Major** | `Graph` does not enforce `UNIQUE (session_id, canonical_key)` | Needs decision before `CockroachStore.flush` |
| M2 | **Major** | `demote()` intentionally violates that UNIQUE — store will reject | Same root as M1; spec tension |
| M3 | **Major** | `InvertedIndex` has no wiring to `Graph` mutations | Owner contract undocumented at branch level |
| M4 | **Major** | `derive()` is not transactional — partial write on mid-call error | Document or make atomic |
| S1 | Minor | `demote()` timestamps with `Utc::now()` vs `derive`/`record_action` using `interaction.created_at` | Determinism / testability |
| S2 | Minor | Synonym raw lookup is case/whitespace-sensitive | Spec-ambiguous; matches fixtures but surprising |
| S3 | Minor | No branch-level integration test covering all three write APIs + index + log | Coverage gap |
| S4 | Suggestion | `CoOccurrence` cap biases early concepts | Spec-allowed; note the policy |
| S5 | Suggestion | Reservations are RAM-local with no durability | Intentional; needs P3 contract note |
| S6 | Info | `ParentOf` dedup + `written_this_call` guard interaction is subtle but correct | Already tested |

---

## Major Findings

### M1 — Graph allows duplicate `canonical_key` (schema says UNIQUE)

**Evidence:** `Graph::insert_concept` inserts by `NodeId` only (`src/graph/graph.rs:397-414`). No check for existing concept with same `(session_id, canonical_key)` and different id. `assert_invariants` checks session, endpoint, weight, chain, Derives coverage, and `Causal/Dependency/Hierarchical` cycles — but not key uniqueness. Schema §4 (`UNIQUE (session_id, canonical_key)`) and `T2.2` / `T2.3` handoffs assume the graph prevents fragmentation, but the enforcement lives only in the *write paths* (`derive`'s `canonicalize` match, `record_action`'s `resolve` dedup). A direct `insert_concept` with a colliding key succeeds in RAM and will fail at `store.flush`.

**Why prior reviews missed it:** T2.1 scoped `Graph` primitives; T2.3/T2.4 scoped their write paths. No task owned the cross-cutting UNIQUE invariant.

**Impact at P2:** Low — all current writers go through the canonical pipeline. **Impact at P3:** `CockroachStore`/`SqliteStore` upsert will error (or silently clobber depending on `ON CONFLICT` choice). Flush retry (§2.4) will spin on a deterministic constraint violation.

**Recommendation:** Either (a) enforce UNIQUE in `Graph::insert_concept` / `Graph::record_edge` path as an `Invariant` error (fail fast, matches DB), or (b) explicitly document that `Graph` is *not* the uniqueness authority and that store adapters must use `INSERT ... ON CONFLICT (session_id, canonical_key) DO NOTHING/UPDATE` with a defined merge policy. Option (a) is safer for v0.1 — it surfaces the bug at the write call, not at flush time. If (b) is chosen, add a `graph::tests::insert_concept_rejects_duplicate_canonical_key` regression.

### M2 — `demote()` explicitly creates duplicate `canonical_key` values

**Evidence:** `demote.rs` module doc: "they do **not** go through the match step ... they are **never deduplicated**, even when two sentences are identical" (`src/graph/demote.rs:8-16`). Two identical sentences produce two concepts with identical `canonical_key` and same `chunk_group_id`. This contradicts `M1`'s UNIQUE and is intentional per spec §7 ("observations are new by construction").

**Spec tension:** §4 DDL says `UNIQUE (session_id, canonical_key)` with no exception; §7 says demoted observations skip matching. Both cannot hold for identical sentences. Fixtures dodge this (observations aren't in `session-rest-api.json`), so the rebuild test doesn't catch it.

**Options:**
1. Relax the DB constraint: `UNIQUE (session_id, canonical_key) WHERE concept_type != 'Observation'` (partial index) — keeps demote's contract, but changes DDL from spec.
2. Make demote's key unique per observation (e.g., append sentence index or hash of content): preserves DDL but changes `canonical_key` semantics for recall.
3. Keep DDL and declare identical-sentence demote as an error (reject or dedup) — contradicts current module doc and test `duplicate sentences not deduped`.

Pick one and update spec §4 + `demote.rs` + the DDL in `store/*` together. Leaving it ambiguous guarantees a flush-time surprise.

### M3 — `InvertedIndex` is not wired to `Graph` — owner must manually sync

**Evidence:** `InvertedIndex::add`/`remove` are pure and correct (`src/graph/index.rs:54-92`), and `T2.6` handoff says "Owns no lock, synchronous, pure; no error paths." But `Graph` mutations (`insert_concept`, `remove_node`, `apply_canonization_transition`) do not call the index, and `derive`/`demote`/`record_action` do not either. The P2 exit criteria say "mutation log ordering verified" and "invariants hold after every test" — both true — but there is no test asserting `index.search` reflects the graph after a `derive`/`demote`/`remove_node` sequence. The T2.6 goldens use `from_snapshot` (bulk load), not incremental `add` after writes.

**Consequence:** `Memory` (P3, the `Arc<RwLock<Graph>>` owner per `src/graph/mod.rs:8-11`) must remember to call `index.add` on every concept create/update and `index.remove` on delete. Nothing in P2 enforces or tests that. A forgotten call is silent — recall just returns stale keyword candidates.

**Recommendation:** Document the owner contract in one place (`src/graph/mod.rs` or `src/store/memory.rs`) and add a branch-level integration test: `derive` 3 concepts → `index.search` finds them → `remove_node` one → search no longer returns it → `demote` adds observations → search finds them. The wiring itself belongs to P3, but the contract belongs to P2.

### M4 — `derive()` leaves partial writes on mid-call error

**Evidence:** `derive.rs` module doc: "`derive` is not transactional: an error mid-call ... leaves earlier writes of that call in place. The graph is never left invariant-violating, though." (`src/graph/derive.rs:72-77`). The only reachable error mid-call today is `ParentOf` reflexive after resolution (`derive.rs:329-340`): concepts in `concepts` + first `ParentOf` pair's nodes may already be inserted before the second pair fails.

**Why it matters:** `derive` is the primary write path agents will call. A caller that retries the same `derive` after an `Invariant` error will create duplicate `CoOccurrence`/`Hierarchical` edges on the partial prefix (reinforcement on retry). The mutation log will contain a half-batch that flushes.

**Recommendation:** Acceptable for v0.1 if documented as the contract (it is, in the module doc). Add one sentence to `PHASE-2-graph-core.md` T2.3 exit criteria: "derive is not atomic; callers must not retry a failed derive with the same `concepts` without reconciling." Alternatively, make it atomic: snapshot `nodes`/`edges`/`edge_keys`/`mutation_log.len`/`epoch` on entry, restore on error — cheap in RAM, but changes the contract. Either way, don't leave the behavior implicit at the branch level.

---

## Minor / Suggestions

### S1 — `demote()` wall-clock vs logical time

`demote` stamps `created_at = Utc::now()` (`src/graph/demote.rs:113`, `handoff: created_at is Utc::now() — pinned signature has no clock param`), while `derive` and `record_action` stamp `interaction.created_at`. This makes `demote` non-deterministic (snapshot diffs, rebuild test would flake if it covered demote) and couples tests to wall-clock. The reservation module solved this by taking `now: DateTime<Utc>` explicitly — same discipline should apply to `demote` when P5/P6 need stable ordering, or at least note it as a known seam. Current tests avoid asserting timestamps, so it's not broken — just inconsistent.

### S2 — Synonym raw lookup sensitivity

`canonical_key` and `canonicalize` do `content.trim()` then exact `synonyms(raw)` lookup (`src/graph/canonical.rs:88-90`, `demote.rs:145-146`). So `"Register_User"` or `" register_user "` (with leading spaces beyond trim) won't match a synonym keyed as `"register_user"`. Fixtures use exact lowercase underscore keys, so it's correct per fixture, but spec §7.1 doesn't state case/whitespace policy. Worth a one-line note in `canonical.rs` that synonym keys are case-sensitive and whitespace-trimmed only.

### S3 — No cross-module integration test

Each T2.x has unit tests ending with `assert_invariants()`, and `tests/rebuild_session.rs` rebuilds fixtures via `derive` + `Graph::upsert_edge`. Nothing exercises `derive` → `record_action` → `demote` → `reserve` → `snapshot` → `from_snapshot` → `assert_invariants` in one graph, nor the mutation log interleaving across those modules, nor `InvertedIndex` incremental sync. Add one `tests/p2_integration.rs` (or extend `rebuild_session.rs`) that does the happy path for all three write APIs and asserts: invariants, `drain_log` chronological order, and index search.

### S4 — `CoOccurrence` cap biases prefix

`max_cooccurrence_per_derive=10` is enforced as "first 10 pairs in call order" (`derive.rs:242-270`). For a `derive` with 8 concepts (28 possible pairs), only pairs among the first ~5 concepts are materialized. Spec §7 says "capped at 10" but not the selection policy. Current policy is deterministic and documented, so not a bug — but downstream recall scoring will see denser connectivity among early concepts in a large derive call. Flag for P5 if recall balancing matters.

### S5 — Reservations have no durability

`Graph::set_reservation`/`clear_reservation` are RAM-local, no `Mutation` kind, round-trip via `GraphSnapshot` only (`src/graph/graph.rs:55-58`, `reserve.rs:1-6`). A writer crash loses reservations. Spec §11 says "expiring" and "advisory," so loss is tolerable — but P3's `MemoryStore`/`CockroachStore` should explicitly state whether `snapshot()` persists reservations durably (it does, via `GraphSnapshot.reservations`) vs whether the mutation log does (it doesn't). Otherwise a restart-then-flush sequence could drop reservations without a snapshot save.

### S6 — `ParentOf` + `written_this_call` interaction

Dedup is on resolved `(parent_node, child_node)` (`derive.rs:307-314`), and the `written_this_call` guard prevents self-reinforcement on canonical-key collisions (`derive.rs:387-395`). The combination is correct and has regression tests (`derive_key_collision_on_preexisting_concept_does_not_self_reinforce`, `derive_parent_of_colliding_pairs_write_one_hierarchical_edge`). No action — noting for traceability since it's the subtlest correctness argument on the branch.

---

## Positive Notes (what went right)

- `drain_log` chronological contract is documented in both `src/graph/mod.rs` and `Graph::drain_log` and locked by `mutation_log_is_chronological_across_interleaved_writes` — P3 can rely on it.
- Lock discipline is structurally enforced: `Graph` owns no lock, all write APIs are `&mut self` + synchronous, zero `.await` in the module — spec §6.4 holds vacuously.
- Canonicalization split-before-lowercase matches fixtures, not the buggy `gen-fixtures.py` order — correctly chosen as frozen truth.
- `derive` re-upsert trick preserves §2.4 node-before-edge ordering for matched concepts — subtle but tested.
- `reserve` checked arithmetic (`from_std` + `checked_add_signed`) prevents panic on adversarial TTL — closed round-1 finding P2 correctly.
- `demote` UAX #29 behavior pinned by tests including the `Dr.` split surprise — prevents future crate-upgrade drift.

---

## Recommended Before Merging to `main`

1. Disposition **M1/M2** together: choose a DDL + `Graph` enforcement strategy and update spec §4 + `graph.rs` + store adapters in one commit. Don't defer past P3.
2. Add branch integration test for **M3** (or ticket it to P3's `Memory` with the contract noted in `src/graph/mod.rs`).
3. Acknowledge **M4** in `PHASE-2-graph-core.md` (one line in T2.3 Done criteria) so callers know retry semantics.

## Re-check Checklist

- [ ] `Graph::insert_concept` canonical_key uniqueness — decision recorded, code or spec updated
- [ ] `demote` vs UNIQUE tension — DDL or module doc reconciled, store adapters aligned
- [ ] `InvertedIndex` owner sync contract — documented + integration test added or ticketed
- [ ] `derive` non-atomicity — acknowledged in phase doc
- [ ] `demote` clock discipline — optional but noted
