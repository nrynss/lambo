# P5 — Recall (read path)

```yaml
id:       P5
branch:   phase/p5-recall
requires: [P1]           # soft: T2.1, T2.6 (works against fixture graphs from day one)
blocks:   P8
parallel: medium   # T5.1 ‖ (T5.2 after T5.1 stubs) ; T5.3 after T5.2 ; T5.4 anytime
runs-parallel-with: P2, P3, P4, P6, P7
```

**Goal:** spec §8, essentially intact. Three phases, a cache, and **the context format —
the single most important piece of output the system produces** (spec §8: it is what
appears on screen in the demo video). On the never-cut list.

Everything is fixture-ok: `fixtures/recall-goldens.json` defines expected behavior against
`session-rest-api` before any live store exists.

---

## Integration contracts from P2 review closes (2026-08-11)

Binding notes for P5 tasks; sources: grok branch review (CLOSED), P2 handoffs.

- **Observation keys may shadow Entity keys (G7 → T5.2 expansion / scoring).** The
  partial-UNIQUE rule allows an Observation and an Entity to share a
  `canonical_key`. Recall must disambiguate by `concept_type` (spec §5 modifiers),
  never by key uniqueness alone.
- **Keyword-source query semantics (G1 → T5.1).** `InvertedIndex::search` counts
  duplicate query tokens ONCE (`search("user user")` scores identically to
  `search("user")`); document-side duplicates still count via tf. Phase-1 scores
  are BM25 OR-sum over unique query terms.
- **`chunk_group_id` is guaranteed non-empty (G3 → T5.2).** `demote` rejects empty
  ids up front, so sibling force-inclusion can rely on a meaningful grouping key.
- **CoOccurrence prefix bias (S4 → T5.3 scoring).** The `max_cooccurrence_per_derive`
  cap materializes pairs among EARLY concepts of a large derive call; connectivity
  is denser there. Flag if recall balancing ever matters.

---

### T5.1 — Phase 1: candidates
```yaml
requires:   T1.1, T2.6
fixture-ok: yes
owns:       src/recall/candidates.rs
status:     done (2026-08-12, reviewed ACCEPT; merged e230f71)
```
Union of: BM25 keyword hits from the in-memory index; concepts of the N=3 most recent
interactions; and — when embeddings are enabled — `store.vector_candidates()`. The vector
leg goes through the `GraphStore` trait so it works the moment T7.3 lands, and degrades to
absent when the capability is missing (log once, spec §3.2). **The vector call is I/O:
gather it before taking the graph lock.**

**Done when:** phase-1 goldens pass keyword-only, and the vector leg is exercised with
`MemoryStore` faked to advertise `VECTOR_SEARCH`.

---

### T5.2 — Phase 2: expansion
```yaml
requires:   T5.1
fixture-ok: yes
owns:       src/recall/expand.rs
status:     done (2026-08-12, reviewed ACCEPT; merged 53979b2)
```
BFS from candidates to `traversal_depth=2`, edge priority
`Dependency`/`Causal` → `Hierarchical` → `CoOccurrence` → `Semantic`, visited-set cycle
guard, `chunk_group_id` siblings force-included but scored independently (T2.5's field).

**Done when:** phase-2 goldens pass, including the sibling-inclusion case.

---

### T5.3 — Phase 3: scoring, assembly & context format ★★
```yaml
requires:   T5.2       # + T4.2 for hot-list; stub behind a trait until it lands
fixture-ok: yes
owns:       src/recall/assemble.rs, src/recall/format.rs
status:     done (2026-08-12, reviewed ACCEPT after 1 remediation round; merged 33fb935)
```
`final_score = daemon_score × w_daemon + query_relevance × w_query`. Hot-listed nodes
within the expanded set force-included **after condition re-validation** (call T4.2's
`revalidate`). Assembly to `max_tokens` via `ceil(bytes / 3.5)` or caller `token_fn`.

**T4.2 `revalidate` signature (revised, XP-3 — 2026-08-12):**
`revalidate(&mut self, graph: &Graph, node: NodeId, now: DateTime<Utc>) -> bool`. Pass
**recall's own `now`** — the predicate re-derives its recency window from it, so an entry
whose window elapsed between detection and this read is dropped here instead of surviving
against a captured instant. On `true`, the entry's `payload` has just been rebuilt against
that `now`, so render it directly: `seconds_ago` is the age at *read* time. Do not cache the
payload across reads. Per-entry re-validation is per-node (one neighborhood walk), so
force-including a handful of hot nodes under the graph lock is cheap.

The context format is v0.6.0 §9.2 verbatim — includes the `[canonical]` marker, the
blast-radius warning line
(`⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.`), the conflict
warning with agent + seconds-ago (T4.3's payload), and active reservations (T2.7). Golden-
file test the exact rendered block for the demo scenario: **this text is the product.**

**Done when:** the demo query `"update user schema"` against `session-rest-api` renders,
byte-for-byte, the golden block showing `[Entity, canonical]`, the ⚑ line, and the conflict
warning.

---

### T5.4 — Recall cache
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/recall/cache.rs
status:     done (2026-08-12, reviewed ACCEPT; merged e5dde1a)
```
LRU keyed `(query_hash, top_k, traversal_depth, mutation_epoch)`. Epoch invalidation only —
no generation counters (arena is gone). Small, boring, independent: a good first task for
an agent waiting on T5.1.

**Done when:** hit/miss/evict tested; any mutation (epoch bump) invalidates.

---

## Exit criteria

- [ ] All recall goldens green against fixture graphs
- [ ] Context-format golden byte-exact
- [ ] Hybrid leg proven behind capability gate (fake VECTOR_SEARCH on/off)
- [ ] Recall performs zero store I/O when capabilities lack VECTOR_SEARCH (RAM-tier promise)

---

## Handoff Log

**What exists now (wave A, integrated e230f71 / 53979b2):**

- `src/recall/candidates.rs` (T5.1) — phase-1 candidates: `gather(&dyn GraphStore, session, embedding, limit) -> Phase1Input` (async; the only store I/O — the vector leg, capability-gated; absent `VECTOR_SEARCH` → zero store calls + one log line) then `candidates(&Graph, &InvertedIndex, Phase1Input, query, limit) -> Vec<Scored<NodeId>>` (sync, safe under the graph lock). Union = BM25 keyword hits ∪ concepts of the 3 most recent interactions (`created_at` desc, ties by id asc) ∪ vector hits; per-node max-merge; score-desc then id-asc total order; `RECENT_SCORE = 0.5`.
- `src/recall/cache.rs` (T5.4) — LRU keyed `(query_hash, top_k, traversal_depth, mutation_epoch)`; capacity const 128; epoch invalidation only; plain struct (caller wires locking).
- Module declarations: `pub mod cache;` + `pub mod candidates;` added to `src/recall/mod.rs` by the two task branches (shared-file policy announcement — additive only).

**Reconciliations (phase doc / fixture note vs shipped contract):**

- **Keyword leg is the BM25 index, not the store substring path.** The `fixtures/recall-goldens.json` note ("EXACT under MemoryStore keyword_candidates, substring on content/canonical_key") predates the P2 index; the operative contract — proven by `recall_phase1_keyword_goldens_pass` (src/graph/index.rs) and `golden_keyword_leg_exact_within_union` (candidates.rs) — is `InvertedIndex::search` (BM25, stemmed, dedup'd query terms). `GraphStore::keyword_candidates` is NOT part of phase 1.
- **Recent-interactions leg:** the 3 most recent by `created_at`, NOT chain order (the fixture's chain tail is the oldest); ties by id asc.

**Recorded but not remediated (P3 doc-accuracy, verdict ACCEPT):** T54-1 (eviction-cost doc claim; capacity small, harmless), T54-2 ("Not Sync-friendly" phrasing; the struct is auto-Sync, real constraint is the `&mut self` API). See `adve-review-t5.4-cache.md`.

**What exists now (wave B, integrated 33fb935):**

- `src/recall/expand.rs` (T5.2) — phase-2 expansion: `expand(graph: &Graph, candidates: Vec<Scored<NodeId>>, depth: usize) -> ExpandedSet` (sync, zero I/O, lock-safe). `ExpandedSet { required, siblings }` — required = candidates (level 0, phase-1 scores carried) + BFS-reached concepts in deterministic discovery order (UNSCORED until T5.3); siblings = force-included `chunk_group_id` concepts (transitive closure over the group, id-asc, NOT BFS-expanded — a group tag is not a graph path). BFS follows `TRAVERSAL_ORDER = [Dependency, Causal, Hierarchical, CoOccurrence, Semantic]` (Derives/Temporal excluded, golden-pinned rationale); visited-set, first-discovery-wins, no re-expansion; `DEFAULT_TRAVERSAL_DEPTH = 2`; `UNSCORED = 0.0`.
- Module declaration: `pub mod expand;` added to `src/recall/mod.rs` (shared-file policy announcement).

**Wave-barrier gates (integrator, 2026-08-12):** rustfmt pass on wave-A files (`cb9c478` — the per-task `cargo check` reviews do not run the fmt/clippy gates; cache.rs/candidates.rs had drifted) and two clippy fixes (`e1414d5` — `len_without_is_empty` on `RecallCache`, `unnecessary_sort_by` in a candidates.rs test). Full default-tier gates green at the barrier: fmt, clippy `--all-targets -D warnings`, `cargo test --all` 395/0, no-default `store-memory`/`store-sqlite` `--all-targets` clean.

**What exists now (wave C, integrated e5dde1a):**

- `src/recall/assemble.rs` (T5.3) — `assemble<F>(graph, expanded, phase1, scores, hot, query, weights, now, token_fn) -> RecallResult`: final = daemon×w_daemon + relevance×w_query for every expanded member (required + siblings, scored independently); relevance = phase-1 score for keyword hits, 0.0 for BFS/sibling members; daemon missing -> 0.0; weights sanitized; score-desc/id-asc sort. Hot members force-included AFTER `HotList::revalidate(graph, node, now)` with recall's own `now`; lapsed dropped; payload rendered at read time. Assembly to `max_tokens` via `default_token_count` (ceil(bytes/3.5)) or caller `token_fn`; whole blocks only, longest score-ordered prefix; truncated blocks keep hits + warnings. `#[allow(clippy::too_many_arguments)]` (Wave-D entry bundles deps).
- `src/recall/format.rs` (T5.3) — `blast_radius(graph, node) -> u64` (spec §4.1 + errata: inbound structural edges Dependency/Causal/Hierarchical, 1-hop, no recursion, Derives/Temporal excluded); `concept_label` with `[canonical]` marker; `blast_radius_warning`, `conflict_warning` (writer, never agents[0] — ALGO-2), `reservation_warning`; `render_context`.
- `fixtures/recall-context-golden.txt` (T5.3) — byte-exact golden for the demo query "update user schema" (top_k=5, max_tokens=500, depth=2), wall-clock-free (fixed planted `now`).
- Module declarations: `pub mod assemble;` + `pub mod format;` (shared-file policy announcement).

**Spec-vs-data reconciliation (blast radius 8 vs 9):** the ⚑ line renders the GRAPH-COMPUTED dependent count, which on `session-rest-api.json` is **8** (pinned by fixtures.rs `Some(8)`, the Cockroach anchor, and gen-fixtures "blast_radius = 8 > 5"), not the 9 in spec §13 / PHASE-8 narration. The "9" belongs to the T8.4 live demo graph, which must plant a 9th dependent (or the fixture gains a 9th orphan plus updated pins — out of P5 scope). Format stays graph-truthful; golden pins 8.

**Wave-barrier gates (integrator, 2026-08-12):** clippy `too_many_arguments` allow on `assemble` (repo precedent) + full no-default TEST matrix caught two T5.1 fixture-dependent tests running un-gated under no-default rows (fixture tests + `load_rest_api_fixture` gated, `629a61c`). Barrier gates: fmt clean; clippy `--all-targets -D warnings` clean; default 412/0; sqlite 444/0; sqlite-minimal 340/0; cockroach 335/0; minimal + demo checks clean.

**Open:** recall() entry-point wiring (Wave D, integrator).
