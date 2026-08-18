# F — SQLite vector search (issue #5)

**Goal:** semantic recall on the local store.

**Why this is not optional.** Spec §3.3 makes `SqliteStore` the local single-machine backend, and
§3.1 exposes `lambo_recall(query, strategy?: "Canonical" | "Hybrid")` to the companion. But
`SqliteStore` reports `Capabilities::empty()` (`src/store/sqlite.rs:564`), so hybrid matching
degrades to keyword-only:

```
hybrid matching disabled: store lacks VECTOR_SEARCH — degrading to
MatchStrategy::Canonical (creating keyword-only concept)
```

Cockroach is the only adapter advertising `VECTOR_SEARCH`. So today, Mooshik's default local mode
— the `$0 / 0ms`, offline, always-on posture the whole §1.3 positioning table rests on — has **no
semantic recall at all**, and "memory recall" is a property of the cloud tier. That inverts the
product.

---

## Why it is smaller than it looks

* The embeddings are **already on disk**: `concepts.embedding BLOB`
  (`migrations/sqlite/001_init.sql:82`), written on every flush for round-trip parity (CON-8). The
  schema comment states the gap outright — *"no VECTOR_SEARCH … the column is never queried."*
* The session's embedding contract is already persisted in `sessions.embedding_{kind,model,dim}`.
* **No migration is required.** Only the query path is absent.
* The reference implementation is in-tree: `VectorSearchStore` (`src/memory.rs:3204`) is
  MemoryStore plus exact cosine over flushed embeddings, built so a default `cargo test` can
  assert derive → flush → vector recall with no live cluster. It is the shape SQLite needs.
* The math exists: `crate::embed::cosine` (`src/embed/math.rs:5`).

---

## F1 — The query path

Implement `vector_candidates` and `vector_candidates_checked` (`src/store/mod.rs:165`, `:182`):
load the session's non-null embeddings, score with `cosine`, sort, return top-k as
`Scored<NodeId>`.

**`vector_candidates_checked` is the one that matters.** The trait documents `vector_candidates`
as unable to bind the query's embedding contract to the durable read, and says new production code
must call the checked variant. It must compare the expected contract to the session's durable
contract *in the same transactional snapshot as every candidate query* and refuse a mismatch —
the way the Cockroach path does.

### Exact scan, not an index

Lambo is session-scoped, so n stays small by construction. At 1024 f32 a concept vector is 4 KB:
the `cloudops-exhibit` session holds 41 concepts, and the K=12 concurrency run produced roughly
1,400 (about 5.7 MB). A full scan at that size is sub-millisecond and returns *exact* nearest
neighbours, where C-SPANN measured 0.99 recall@50 at beam 64.

`sqlite-vec` gives a real ANN index and can be statically linked via `sqlite3_auto_extension`, so
the objection is not a stray `.so` — it is a C toolchain dependency across four cross-compiled
release targets plus auto-extension registration before sqlx opens a pool. Not worth it until an
exact scan actually hurts. **Trigger to revisit:** per-session concept counts where the scan shows
up in recall latency. That is a number, not a guess.

**Depends on:** nothing.

---

## F2 — Advertise the capability

Flip `capabilities()` to `VECTOR_SEARCH` and implement `vector_dimensions()` **in the same
change**. `check_vector_search_contract` (`src/resolve.rs:63`) refuses a store that claims the
capability without a concrete width, and a test already asserts that refusal (`resolve.rs:413`).

**The sharper trap.** `vector_candidates_checked` has a fail-closed *default* that returns
`StoreError::Capability` when the store advertises `VECTOR_SEARCH` — it exists so third-party
v0.2.0 adapters cannot silently weaken the contract. So flipping `capabilities()` before F1 is
complete does not degrade recall to keyword-only; it makes recall **error**. F1 lands first, or
the two land together. Never F2 alone.

**Depends on:** F1.

---

## Amendment to issue #5 as filed

The issue specifies `vector_dimensions() -> Some(1024)`. **Do not.** That introduces a third
hardcoded width — beside `VECTOR(1024)` in the Cockroach migration and the `1024` default in
`EmbedderConfig` — at exactly the moment B is removing the hardcoding, and Mooshik's Gemini
embedder will not be 1024 (768/1536/3072 only).

The width must come from the persisted session contract (`sessions.embedding_dim`, already
stored) or from config, matching whatever B2 settles on as the authority.

---

## Done when

- [ ] SQLite advertises `VECTOR_SEARCH` and reports a width that is not hardcoded
- [ ] Hybrid derive no longer logs the degradation warning on SQLite
- [ ] A test proves the vector leg **fired**, rather than inferring it from rank — mirroring what
      `VectorSearchStore` does for MemoryStore
- [ ] Contract mismatch is refused on the checked path
- [ ] Recall parity between SQLite and Cockroach on the same seeded graph, with any divergence
      explained by ANN approximation rather than by the adapter
- [ ] `README.md` and the site's End-to-end page describe SQLite as the out-of-the-box store;
      revisit what each tier earns once semantic matching works there
