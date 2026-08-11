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
status:     not-started
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
status:     not-started
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
status:     not-started
```
`final_score = daemon_score × w_daemon + query_relevance × w_query`. Hot-listed nodes
within the expanded set force-included **after condition re-validation** (call T4.2's
`revalidate`). Assembly to `max_tokens` via `ceil(bytes / 3.5)` or caller `token_fn`.

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
status:     not-started
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

> _Fill on completion._
