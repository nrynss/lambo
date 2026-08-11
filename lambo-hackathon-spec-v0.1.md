# Lambo v0.1-hackathon — Implementation Spec

**Target:** CockroachDB × AWS Hackathon, submission deadline August 18, 2026, 5:00 pm ET
**Derived from:** Lambo spec v0.6.0
**Status:** Implementation-ready. Frozen for the duration of the build.

---

## 1. What This Document Is

v0.6.0 is a 22,000-word design for an embedded Rust library with an in-process arena, a
concurrency protocol, and a full GC lifecycle. This document is the **narrow slice of that
design that ships in nine days**: Rust, no PyO3, a single binary, with a pluggable durable
store underneath.

**On the language.** The point of Rust here is a single performant binary, not raw speed —
`lambo serve` is an MCP server that people wire into Claude Code by pasting a path into a
JSON config, and "install Python first" is where that adoption dies. The expensive part of
the Rust route is the FFI boundary, and v0.1 doesn't need it: the demo app is a hosted
client, agents talk over stdio or HTTP, and nothing embeds in a Python process. PyO3 wheels
are deferred to v0.7.0, where they are a distribution decision rather than a prerequisite.

It is not a summary of v0.6.0 and it is not a replacement for it. Where the two disagree,
v0.6.0 remains the design of record and this document is the deliberate compromise.
Everything cut is listed in §13 and returns in v0.7.0.

Three things must survive the cut intact, because they are what make Lambo distinct from
flat retrieval memory:

1. **The bipartite graph** — interactions and concepts as co-equal nodes, giving provenance
   and temporal semantic bridging without embeddings.
2. **Canonization** — importance earned through structural evidence, not declared at write time.
3. **Consequence awareness** — blast radius surfaced *before* a destructive action, not after.

If a scope decision threatens one of these, cut something else.

---

## 2. Architecture

### 2.1 Tier placement

v0.6.0 §4.1 places Lambo at the RAM tier: microsecond recall, session-scoped, with a
persistent store below it. Putting the graph in a network database inverts that. The
resolution:

> **The in-memory graph is primary. The durable store is synchronised behind it.**

- `recall()`, `derive()`, and daemon scoring operate **entirely against RAM**. No network
  round-trip on the hot path. Microsecond latency preserved.
- Mutations are batched and flushed **asynchronously** to the store (write-behind, not
  write-through).
- Queries that are structural rather than latency-critical — blast radius, interaction span,
  temporal coverage, cross-session lookup, audit — execute **as SQL against the store**.

That last point is what makes the store more than a checkpoint blob. Slow structural
questions live in the database; fast recall lives in RAM. The tier boundary is real.

### 2.2 Single writer, many readers — the deployment model

Two processes holding RAM copies of the same session would diverge, and nothing in v0.6.0
resolves that. Because Lambo ships as a binary rather than a library, this isn't a caveat to
disclose — it's how the thing is deployed.

- **One `lambo serve` process owns a session.** Agents connect to it over MCP (stdio or
  HTTP) and are tasks within it, exactly as v0.6.0 §14.1 describes. The demo's two agents
  share one process.
- **Any number of readers** may query the store directly — other tools, dashboards, the
  CockroachDB MCP server. They see eventually consistent state, bounded by
  `backend_flush_interval`.
- Readers never write. There is no coordination protocol and no merge.

Multi-writer coordination is the Lambo Cloud problem and is out of scope.

### 2.3 Durability mode

Rather than introducing a new concept, this slots into v0.6.0 §6.4 as a fourth mode:

| Mode | Behavior | v0.1 |
|------|----------|------|
| `none` | Pure RAM | supported |
| `checkpoint` | Periodic disk snapshot | **removed** |
| `wal` | Checkpoint + write-ahead log | **removed** |
| `backend` | Write-behind sync to a `GraphStore` | **new, default** |

`backend_flush_interval` (default: 1.0s) replaces `wal_flush_interval`.
`backend_flush_max_batch` (default: 500 mutations) forces an early flush.

### 2.4 Flush semantics

The writer maintains an ordered mutation log. On flush:

1. Drain the log into a batch.
2. Open a transaction on the store.
3. Apply node upserts, then edge upserts, then deletions, then canonization transitions.
4. Commit.

Ordering within a batch matters (edges reference nodes). Ordering *between* batches is
guaranteed by the single-writer rule.

**On flush failure:** retry with exponential backoff up to `backend_flush_retries`
(default: 3). The in-memory graph is unaffected — the session keeps working. After
exhausting retries the batch is retained, a `BackendFlushFailed` warning is raised, and the
mutation log continues accumulating. If the log exceeds `backend_log_max` (default: 50,000)
the session degrades to `durability="none"` and logs at `ERROR`. **Data loss on writer crash
is bounded by the flush interval**, and `stats()` exposes current lag and log depth so the
bound is observable rather than assumed.

### 2.5 Startup

`Memory(session=..., durability="backend")` calls `store.load_session(session_id)`. If the
session exists, the full graph is materialised into RAM and the daemon runs its warm-up
rescore (v0.6.0 §7.1). If not, an empty graph is created. Sessions larger than RAM are not
a v0.1 concern.

---

## 3. Storage Abstraction

### 3.1 Design rule

**The interface speaks Lambo's vocabulary, never the database's.** No `create_branch()`, no
`begin_transaction()`, no dialect leaking upward. If an adapter cannot answer a question
natively it computes it — the caller never learns which.

This is what keeps a future Dolt adapter (branch-based blast radius) and the CockroachDB
adapter (transactional rollback or a direct query) behind the same method.

### 3.2 Interface

```rust
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn init_schema(&self) -> Result<(), StoreError>;
    fn capabilities(&self) -> Capabilities;   // VECTOR_SEARCH | HISTORY

    // Sync
    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError>;
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError>;

    // Candidate retrieval
    async fn keyword_candidates(
        &self, session: &SessionId, tokens: &[String], limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    async fn vector_candidates(           // requires Capabilities::VECTOR_SEARCH
        &self, session: &SessionId, embedding: &[f32], limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    // Structural queries (canonization)
    async fn blast_radius(
        &self, session: &SessionId, node: NodeId, min_edge_age: Duration,
    ) -> Result<u64, StoreError>;

    async fn interaction_span(
        &self, session: &SessionId, node: NodeId, min_age: Duration,
    ) -> Result<InteractionSpan, StoreError>;   // { distinct: u64, coverage: f64 }

    // Audit
    async fn record_canonization(
        &self, event: &CanonizationEvent,
    ) -> Result<(), StoreError>;
}
```

`vector_candidates` is gated behind a capability rather than assumed. An adapter without it
degrades to keyword-only matching; `MatchStrategy::Hybrid` falls back to
`MatchStrategy::Canonical` and logs once.

**Query construction:** runtime `sqlx::query` throughout, not the compile-time macros.
Two backends with different placeholder and interval syntax makes macro checking more
trouble than it's worth at this scale. Each adapter owns its SQL.

### 3.3 Adapters

| Adapter | Cargo feature | Config `kind` | Purpose | v0.1 |
|---------|---------------|---------------|---------|------|
| `MemoryStore` | `store-memory` (default) | `memory` | Unit tests, fixture-ok tracks, no I/O | ships |
| `CockroachStore` | `store-cockroach` | `cockroach` | Hackathon primary, distributed vector index | ships |
| `SqliteStore` | `store-sqlite` | `sqlite` | Fast tests, local-first embedded tier | ships |
| `DoltStore` | *(v0.7)* | `dolt` | Versioned memory, branch-based blast radius | deferred |

| Embedder | Cargo feature | Config `kind` | Purpose | v0.1 |
|----------|---------------|---------------|---------|------|
| `BgeM3LlamaCppEmbedder` | `embed-bge` (default) | `bge_m3` | Default dense path (HF + llama.cpp) | ships |
| `FixtureEmbedder` | `embed-fixture` (default) | `fixture` | Deterministic tests / CI | ships |
| `BedrockEmbedder` | `embed-bedrock` | `bedrock` | Titan V2 when account authorized | ships when unlocked |

SQLite is the second store adapter rather than Dolt because it proves the abstraction is
real, keeps the test suite fast, and restores the embedded tier v0.6.0 was written for.
Dolt's divergence is mostly mechanical (placeholders, upsert syntax) with one genuine gap
(`VEC_DISTANCE()` vs Cockroach's vector index) — real work, but not hackathon work.

**Construction:** process start calls `resolve_backends` / `resolve_from_config_path`
(returns `ResolvedBackends`). Low-level `build_store` / `build_embedder` are for registries
and unit tests. Application code must not construct concrete adapter types outside those
paths.

**Vector width:** not a global constant. `GraphStore::vector_dimensions()` returns
`Some(n)` when the adapter persists dense vectors (Cockroach `VECTOR(n)`), else `None`.
`resolve::check_vector_compatibility` requires the embedder’s `dimensions()` to match when
the store declares a width. Config `embedder.dim` is the embedder’s expected output
(default 1024 for BGE demos).

**Embedding space:** `GraphSnapshot.embedding: Option<EmbeddingContract { kind, model, dim }>`
stamps the active model space; refuse mid-session kind/model/dim changes without re-embed.

### 3.4 Packaging — Level B pluggability (errata 2026-08-11)

Adapters are **not** always linked into every binary. v0.1 uses **Level B** packaging:

1. **Cargo features** compile adapters in (tables in §3.3). Optional crates (`sqlx`, AWS
   SDK, `reqwest` for BGE) are pulled only by the matching feature.
2. **`lambo.toml` and/or env** select among compiled kinds at process start.
   - File: `[store]` / `[embedder]` sections (example: `lambo.example.toml`).
   - Env: `LAMBO_STORE`, `LAMBO_EMBEDDER`, `LAMBO_COCKROACH_DSN`, `LAMBO_LLAMA_EMBED_URL`,
     `LAMBO_CONFIG`, … — **non-empty** env **overrides** file when set.
   - Unknown TOML keys are rejected (`deny_unknown_fields`).
3. Selecting a kind that is not compiled in is a **hard error** with a rebuild hint
   (`--features store-cockroach`), never a silent fallback to memory/keyword.
4. **Default feature set** for everyday `cargo test`: `store-memory`, `embed-bge`,
   `embed-fixture`, `fixtures`. **Demo/ship profile:** enable `store-cockroach` (and
   `embed-bedrock` when authorized), or use the convenience feature `demo`.
5. **Single construction site:** CLI/`serve` resolve once into `ResolvedBackends` and pass
   that into the command body (no double `build_*`).

This keeps CI and offline tracks free of Cockroach/AWS weight while the demo binary enables
the durable path. Design of record: `dev-diary/notes/level-b-pluggability.md`. Runtime
plugins (dynlib / sidecar, Level C) are **out of v0.1 scope**.

---

## 4. Schema

Backend-neutral DDL. CockroachDB-specific clauses marked.

```sql
CREATE TABLE sessions (
    session_id      STRING PRIMARY KEY,
    root_goal       JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ
);

CREATE TABLE interactions (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    agent_id        STRING NOT NULL,
    prompt_text     STRING,
    previous_id     UUID REFERENCES interactions(id),
    created_at      TIMESTAMPTZ NOT NULL,
    INDEX (session_id, created_at)
);

CREATE TABLE concepts (
    id                  UUID PRIMARY KEY,
    session_id          STRING NOT NULL REFERENCES sessions(session_id),
    content             STRING NOT NULL,
    canonical_key       STRING NOT NULL,
    concept_type        STRING NOT NULL,     -- Entity|Logic|Constraint|Resource|Observation
    origin_interaction  UUID NOT NULL REFERENCES interactions(id),
    origin_agent        STRING NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL,
    access_count        INT NOT NULL DEFAULT 0,
    last_accessed       TIMESTAMPTZ,
    gc_survived         INT NOT NULL DEFAULT 0,
    canonization_status STRING NOT NULL DEFAULT 'None',
    blast_radius        INT,
    last_demotion_time  TIMESTAMPTZ,
    embedding           VECTOR(1024),        -- dense 1024-d (BGE-M3 default / Titan V2 swap-in)
    UNIQUE (session_id, canonical_key),
    INDEX (session_id, canonization_status)
);

-- Errata (2026-08-11, P2 integration / muse-spark M1-M2): the UNIQUE above is
-- **partial** — it constrains non-Observation concepts only:
--     CREATE UNIQUE INDEX concepts_key_non_obs_idx
--       ON concepts (session_id, canonical_key)
--       WHERE concept_type <> 'Observation';
-- Demoted Observations (spec §7) skip the match step and may legitimately
-- share a canonical key (identical sentences from different chunks are distinct
-- context-overflow records). `Graph::insert_concept` and
-- `Graph::assert_invariants` enforce the same rule in RAM.

-- CockroachDB only
CREATE VECTOR INDEX concepts_embedding_idx ON concepts (embedding);

CREATE TABLE edges (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    source          UUID NOT NULL,
    target          UUID NOT NULL,
    edge_type       STRING NOT NULL,
    weight          FLOAT NOT NULL,
    reinforcements  INT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL,
    last_reinforced TIMESTAMPTZ NOT NULL,
    UNIQUE (source, target, edge_type),
    INDEX (session_id, target, edge_type),
    INDEX (session_id, source, edge_type)
);

CREATE TABLE synonyms (
    session_id      STRING NOT NULL REFERENCES sessions(session_id),
    source_key      STRING NOT NULL,
    canonical_key   STRING NOT NULL,
    PRIMARY KEY (session_id, source_key)
);

CREATE TABLE canonization_events (
    id              UUID PRIMARY KEY,
    session_id      STRING NOT NULL,
    node_id         UUID NOT NULL,
    from_status     STRING NOT NULL,
    to_status       STRING NOT NULL,
    blast_radius    INT,
    occurred_at     TIMESTAMPTZ NOT NULL,
    INDEX (session_id, occurred_at)
);

CREATE TABLE reservations (
    session_id      STRING NOT NULL,
    node_id         UUID NOT NULL,
    agent_id        STRING NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, node_id)
);
```

`canonization_events` is not in v0.6.0. It exists because canonization is the thing worth
showing, and an append-only transition log makes it visible in the UI, in the video, and
through the CockroachDB MCP server. It also foreshadows the Dolt history story in v0.7.0.

`edges.source` and `edges.target` deliberately carry no foreign key — they reference either
table, and enforcing that would need a polymorphic constraint not worth the schema cost at
this scale. The writer enforces it.

### 4.1 Structural queries

**Blast radius** (v0.6.0 §7.9 Stage 3: nodes with zero inbound edges after hypothetical
removal — no recursion needed, the definition is 1-hop).

**Errata (2026-08-11, T1.4 / MemoryStore):** count only concept-to-concept structural edges
(`Dependency` / `Causal` / `Hierarchical`). Spec §5.7 requires a `Derives` edge
(interaction → concept) on every concept; treating that provenance edge as "another inbound
source" would zero Stage-3 blast radius for every legal graph. `Temporal` edges are likewise
excluded. Source must be a concept id (not an interaction).

```sql
SELECT count(*)
FROM concepts c
WHERE c.session_id = $1
  AND EXISTS (
      SELECT 1 FROM edges e
      JOIN concepts src ON src.id = e.source
      WHERE e.target = c.id AND e.source = $2
        AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND e.created_at <= now() - ($3 || ' seconds')::INTERVAL
  )
  AND NOT EXISTS (
      SELECT 1 FROM edges e2
      JOIN concepts src2 ON src2.id = e2.source
      WHERE e2.target = c.id AND e2.source <> $2
        AND e2.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND e2.created_at <= now() - ($3 || ' seconds')::INTERVAL
  );
```

**Interaction span and temporal coverage** (Stage 2 — traces concept-to-concept edges back
through `origin_interaction`, per v0.6.0's clarification):

```sql
WITH span AS (
    SELECT DISTINCT i.id, i.created_at
    FROM edges e
    JOIN concepts src ON src.id = e.source
    JOIN interactions i ON i.id = src.origin_interaction
    WHERE e.target = $2
      AND e.session_id = $1
      AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
      AND i.created_at <= now() - ($3 || ' seconds')::INTERVAL
),
extent AS (
    SELECT min(created_at) AS lo, max(created_at) AS hi
    FROM interactions WHERE session_id = $1
)
SELECT
    (SELECT count(*) FROM span),
    CASE WHEN extract(epoch FROM (extent.hi - extent.lo)) > 0
         THEN extract(epoch FROM (
                  (SELECT max(created_at) FROM span) -
                  (SELECT min(created_at) FROM span)))
              / extract(epoch FROM (extent.hi - extent.lo))
         ELSE 0.0 END
FROM extent;
```

Both are pure SQL, both port to SQLite with placeholder and interval-syntax substitution,
and both are exactly the kind of query the judging criteria mean by "more than toy queries."

---

## 5. Graph Model

Unchanged from v0.6.0 §5.1–5.4 except as noted.

**Node types:** interaction, concept. Bipartite. Unchanged.

**Concept types:** all five retained (`Entity`, `Logic`, `Constraint`, `Resource`,
`Observation`) with their eviction resistances and score multipliers.

**Edge types:** seven of nine retained.

| Type | Connects | Decay | Retained |
|------|----------|-------|----------|
| `Temporal` | Interaction → Interaction | no | yes |
| `Derives` | Interaction → Concept | no | yes |
| `CoOccurrence` | Concept → Concept | yes | yes |
| `Causal` | Concept → Concept | no | yes |
| `Dependency` | Concept → Concept | no | yes |
| `Hierarchical` | Concept → Concept | no | yes |
| `Semantic` | Concept → Concept | yes | yes |
| `CrossOccurrence` | — | — | **cut** |
| `Fixes` | — | — | **cut** |

`CrossOccurrence` is cut because v0.6.0 §19 lists its value as an unvalidated open question
and it is disabled by default in the `analytical` profile — the only profile v0.1 ships.
`Fixes` is cut for low usage.

**Weight dynamics:** v0.6.0 §5.4 unchanged, including the design decision that recall does
not reinforce edges.

**Invariants (v0.6.0 §5.7), reduced:**
- Every non-first interaction node has exactly one `Temporal` predecessor.
- Every concept node has at least one `Derives` edge.
- No duplicate `(source, target, edge_type)`.
- No cycles in `Causal` or `Dependency`, enforced at write time by BFS.
- Weights ≥ 0 and finite; composite scores finite (NaN/Inf clamped to 0.0).

Arena and generational indexing (§5.9) are gone — the store issues UUIDs.

---

## 6. API Surface

### 6.1 Library

```rust
// Level B: resolve once (store + embedder + dim/contract), then hand into Memory.
let backends = resolve_from_config_path(None)?;   // lambo.toml + env; fail-closed

let mem = Memory::builder()
    .session("project-doom")
    .agent("agent-A")
    .store(backends.store)
    .embedder(backends.embedder)
    .embedding_contract(backends.embedding)       // stamp / refuse model-space mix
    .match_strategy(MatchStrategy::Hybrid)
    .flush_interval(Duration::from_secs(1))
    .scoring_weights(ScoringWeights::default())   // 0.25 / 0.20 / 0.20 / 0.35
    .build().await?;

mem.set_root_goal(&["doom-style FPS", "3D renderer"])?;
mem.declare_synonym("register_user", "create_user")?;

let result = mem.recall(RecallQuery {
    query: "update user schema",
    top_k: 5, max_tokens: 500, traversal_depth: 2,
})?;

mem.derive(&[
    ("user schema", ConceptType::Entity),
    ("must stay backward compatible", ConceptType::Constraint),
], ParentOf::none())?;

mem.record_action(Action {
    action: "created migrations/003.sql",
    produces: &["migrations/003.sql"],
    depends_on: &["user schema"],
    modifies: &[],
})?;

mem.demote(&chunks, meta)?;

let impact = mem.retract("stale dependency", DryRun::Yes)?;
let reservation = mem.reserve(node_id, Duration::from_secs(30))?;

let saints = mem.canonical_memories()?;
let stats = mem.stats();
mem.close().await?;
```

**Events replace callbacks.** `mem.events() -> Receiver<DaemonEvent>` yields `Conflict`,
`Drift`, `Stale`, `HighRisk`, and `Canonized`. This is the idiomatic Rust shape and it
deletes v0.6.0 §7.6 wholesale — no callback thread pool, no re-entrancy guard, no execution
timeout, no queue bound. The consumer decides when to read. A dropped receiver is not an
error.

**Cut from v0.6.0 §11.1:** `correct()`, `merge_concepts()`, `resume()`, `restart_daemon()`,
`checkpoint()`, and the `PolicyAction` protocol.

### 6.2 Binary

```bash
lambo serve --session S --transport stdio       # MCP server (primary artifact)
lambo serve --session S --transport http --port 7700
lambo serve --config ./lambo.toml --session S   # Level B process file (optional)
lambo demo --scenario rest-api                  # scripted two-agent demo
lambo recall --session S --query Q --top-k 5
lambo saints --session S                        # canonical memories
lambo inspect --session S --focus "user schema" --depth 2
lambo stats --session S
lambo provision                                 # ccloud CLI wrapper, schema bootstrap
```

MCP tools exposed by `lambo serve`: `lambo_recall`, `lambo_derive`, `lambo_record_action`,
`lambo_reserve`, `lambo_inspect`, `lambo_saints`, `lambo_stats`.

Note the distinction from §12.1 — this is *Lambo's* MCP server, which agents use to write
memory. CockroachDB's managed MCP server is separate, read-only, and used to inspect the
store underneath.

Process config (Level B, §3.4): `--config PATH`, else `LAMBO_CONFIG`, else `./lambo.toml`
if present, else defaults. Env vars override file keys. Secrets (DSN passwords) stay in the
environment, not committed TOML.

### 6.3 Crates

Core crates always available. **Adapter crates are optional** and enabled by Cargo features
(§3.4).

| Concern | Crate | Feature gate |
|---------|-------|--------------|
| Async runtime | `tokio` | always |
| Process config | `toml`, `serde` | always |
| MCP | `rmcp` (fall back to hand-rolled stdio JSON-RPC if it fights) | always |
| HTTP (demo app, http transport) | `axum` | always |
| Stemming | `rust-stemmers` (Porter) | always |
| Segmentation | `unicode-segmentation` (UAX #29) | always |
| Locking | `parking_lot` | always |
| CLI | `clap` | always |
| Logging | `tracing`, `tracing-subscriber` | always |
| SQL (Cockroach / SQLite adapters) | `sqlx` (`postgres` and/or `sqlite`, `uuid`, `chrono`) | `store-cockroach` / `store-sqlite` |
| BGE-M3 HTTP client | `reqwest` (rustls) | `embed-bge` |
| Bedrock Titan | `aws-sdk-bedrockruntime`, `aws-config` | `embed-bedrock` |

### 6.4 Concurrency rule

The graph is `Arc<RwLock<Graph>>` using `parking_lot`. Critical sections are short and
**a graph lock is never held across an `.await`**. The daemon and the flush task are tokio
tasks; both take the lock, do their work, and release before any I/O. This is v0.6.0 §14.1
unchanged — the arena, generational indices, control channel, and forced-GC protocol of
§5.9 and §14.3 are all gone with the features that needed them.

---

## 7. Write Path

v0.6.0 §8, reduced.

**`derive()`** — per concept: canonicalize (§8.1 below) → within-call dedup → match or
create → `Derives` edge from current interaction → pairwise `CoOccurrence` edges capped at
`max_cooccurrence_per_derive=10` → `Hierarchical` edges from `parent_of` → enqueue mutation
batch → notify daemon.

**`record_action()`** — creates a `Resource` concept for the action, then `Causal` edges to
`produces` and `modifies`, `Dependency` edges to `depends_on`. Implicit node creation runs
the full canonicalization pipeline. Cycle check by BFS over `Causal`/`Dependency` **after**
canonical resolution.

**`demote()`** — `Observation` nodes from context-overflow chunks, UAX #29 sentence
segmentation, `chunk_group_id` for sibling co-retrieval. Custom `chunk_split_fn` cut.

### 7.1 Canonicalization

v0.6.0 §5.6 steps 1–5, plus step 6.

1. Normalize — lowercase, split hyphens/underscores/camelCase, strip stopwords.
2. Stem — Porter only (`snowball` and custom `stem_fn` cut).
3. Token-sort → canonical key.
4. Synonym resolution — **direct lookup only**. Transitive chains, cycle detection, path
   compression, and the merge token (§14.4) are cut.
5. Match against `canonical_key`.
6. If unmatched and `match_strategy="hybrid"`, embed and check `vector_candidates()` above
   `semantic_match_threshold=0.85`. On match, create a `Semantic` edge.

Step 6 is the vector index doing real work: it merges concepts that normalization cannot
("register user" / "create account"), which is exactly what keeps the graph from
fragmenting into islands.

---

## 8. Read Path

v0.6.0 §9, essentially intact.

**Phase 1 — candidates.** Keyword search over the in-memory inverted index (BM25,
per-session `df`). Plus the concepts of the N=3 most recent interactions. Plus, when
embeddings are enabled, `vector_candidates()` against the store.

**Phase 2 — expansion.** BFS from candidates to `traversal_depth` (default 2), following
edges in priority order: `Dependency`/`Causal` → `Hierarchical` → `CoOccurrence` →
`Semantic`. Visited-set cycle detection. `chunk_group_id` siblings force-included, scored
independently.

**Phase 3 — scoring and assembly.**
`final_score = daemon_score × w_daemon + query_relevance × w_query`.
Hot-listed nodes within the expanded set are force-included after condition re-validation.
Output assembled to `max_tokens` using the built-in `ceil(bytes / 3.5)` estimator or a
caller-supplied `token_fn`.

**Recall cache:** simple LRU keyed by `(query_hash, top_k, traversal_depth, mutation_epoch)`.
The generation-counter validation of v0.6.0 §6.1 is gone with the arena; epoch invalidation
alone is sufficient.

**Context format:** v0.6.0 §9.2 verbatim, including the `[canonical]` marker and the
blast-radius warning line. This block is what appears on screen in the demo video — it is
the single most important piece of output the system produces.

---

## 9. Daemon

A background thread. v0.6.0 §7, reduced.

**Scoring (§7.3), unchanged:**

```
score = recency·0.25 + frequency·0.20 + session_activity·0.20 + density·0.35
        + edge_type_bonus + concept_type_modifier
```

All dimensions clamped to [0,1] before weighting. `centrality_bonus` cut
(`enable_centrality` was already `False` by default).

**Hot list** — bounded priority queue, `hot_list_max=1000`. Entry conditions retained:
conflict detected, high-risk modification, drift detected, stale session. Conditions
re-validated on each `recall()`.

**Conflict detection** — two or more active agents with edges to the same node, at least one
`Causal`/`Dependency` with write activity inside `conflict_recency_window=30s`. This is the
demo's trigger.

**Drift detection (§7.7)** — weighted shortest path over `Causal`/`Dependency`/`Hierarchical`
to any root goal node; warn beyond `drift_threshold=5` hops or no path. Root goal nodes are
automatically `Venerable`. The creative-profile `CoOccurrence` fallback is cut with the
profile.

**GC (§6.6), reduced to periodic only:** every `gc_interval=10,000` mutations —
1. Edge cleanup below `min_edge_weight` past `gc_edge_ttl`.
2. Concept cleanup: orphans and sub-threshold scores, **excluding** `Venerable`, `Canonical`,
   and root goal nodes.
3. Disconnected-component cleanup via BFS from the temporal chain.
4. Index maintenance.
5. **Increment `gc_survived` on all survivors.**
6. Canonical budget enforcement against `max_canonical_nodes`.
7. Increment `MutationEpoch`.

Step 5 is why GC must stay — it is the input to canonization Stage 1. Interaction compaction
(old §6.6 step 4), forced GC, emergency eviction, and the control-channel protocol are all
cut. Capacity is elastic; `max_concept_nodes` becomes advisory and logs a warning rather
than triggering eviction.

**Events** — the daemon publishes `DaemonEvent` on a broadcast channel (§6.1). No callback
dispatch, no pool, no re-entrancy risk. `PolicyAction` and the pause protocol are cut.

---

## 10. Canonization

v0.6.0 §7.9, retained whole. This is the differentiator and it does not get cut.

**Stage 1 — Candidate.** `gc_survived >= 3` AND composite score above the 90th percentile of
non-Canonical peers, evaluated only when at least `canonization_min_peer_count=20`
non-Canonical concepts exist.

**Stage 2 — Venerable.** Inbound `Dependency`/`Causal`/`Hierarchical` edges whose source
concepts trace back to `>= 3` distinct interactions, spanning `>= 0.3` of the session's
temporal extent. Computed by `store.interaction_span()`. Only edges and interactions older
than `canonization_edge_min_age=60s` count — the adversarial inflation guard.
**Venerable nodes are eviction-immune.**

**Stage 3 — Canonical.** Hypothetical removal would orphan `> 5` nodes, computed by
`store.blast_radius()`. Re-promotion blocked for `canonization_repromotion_cooldown=300s`
after any demotion.

**Evaluation** — every `canonization_eval_interval=60s`, at most
`canonization_eval_batch_size=50` Venerable nodes per cycle, round-robin cursor with
score-descending order within the batch. Anti-starvation preserved.

**Canonical nodes are:** eviction-immune; always promoted first with `is_canonical=True`;
marked `[canonical]` in recall output with a blast-radius warning; bounded by
`max_canonical_nodes=1000` with lowest-blast-radius demotion.

**Demotion** sets `last_demotion_time`, nulls `blast_radius`, and writes a row to
`canonization_events`.

The immediate-re-evaluation-after-compaction rule is cut along with compaction. Summary-node
proportional counting is cut for the same reason.

---

## 11. Multi-Agent

v0.6.0 §10.3, reduced but functional — this carries the demo.

- Agents share a session by sharing `session_id`, as threads in the single writer process.
- `origin_agent` recorded on every concept.
- Conflict detection across agents via the daemon.
- **Soft-lock reservations** (§10.3.3) retained: advisory, visible in other agents' recall
  output, expiring after `timeout`. Same-agent re-reservation extends; cross-agent returns
  `AlreadyReserved`.
- **Access levels and floating interaction nodes (§10.3.1) are cut.** All agents have full
  access. Read-only agents are the *reader process* story instead, and read from the store.
- Trust model unchanged: agent identity is caller-supplied and unauthenticated (§10.3.2).

---

## 12. Hackathon Requirements

### 12.1 CockroachDB tools (two required)

| Tool | How it is used |
|------|----------------|
| **Distributed Vector Indexing** | `concepts.embedding VECTOR(1024)` with a vector index. Powers §7.1 step 6 — semantic concept merging that keeps the graph connected when normalization fails. Not a RAG sidecar: the vectors live beside the graph they belong to. |
| **Cloud Managed MCP Server** | Connected read-only to Claude Code. Used to inspect the live memory graph mid-session — query `canonization_events` to watch nodes earn status, and `edges` to trace provenance. This is the reader-process story from §2.2 made concrete. |
| **ccloud CLI** *(third, optional)* | Cluster provisioning and schema bootstrap, scripted in `scripts/provision.sh`. Recorded for the video. |

### 12.2 AWS services (one required)

| Service | How it is used |
|---------|----------------|
| **Amazon Bedrock** | Titan Text Embeddings V2 behind the `Embedder` trait (1024-dim), via `aws-sdk-bedrockruntime`, feature `embed-bedrock`. Default dense path is BGE-M3 (`embed-bge`) until the account is authorized. Claude on Bedrock may still drive the two demo agents. |
| **AWS Lambda** *(optional)* | Scheduled canonization sweep against the store for sessions with no active writer. |

### 12.3 Judging criteria alignment

- *Agentic Memory Design* — the store holds normalized graph structure, embeddings, and an
  append-only canonization audit trail. Blast radius and interaction span run as SQL.
- *Technological Implementation* — the store is behind a capability-gated interface with two
  working adapters; MCP access is read-only by default.
- *Real-World Impact* — the demo prevents a destructive schema change that flat retrieval
  memory would not catch.
- *Product Readiness* — ships as a single binary with no runtime dependency; flush failure is
  bounded and observable; single-writer is a stated deployment model, not a hidden limit;
  `stats()` exposes lag and backlog.
- *Creativity & Originality* — canonization. Nobody else ships memory that decides for itself
  what is load-bearing.

### 12.4 Deliverables

- [ ] Public repo, MIT license detectable in the About section
- [ ] README with setup and run instructions, and the single-writer constraint stated
- [ ] Functional demo app URL
- [ ] Video under 3 minutes, showing the memory layer at work
- [ ] Written identification of CockroachDB tools and AWS services used
- [ ] Architecture diagram *(optional, cheap, do it)*

All code written during the submission period. The v0.6.0 spec is prior design work, not
incorporated code — no disclosure obligation, but the README should credit it for honesty.

---

## 13. Demo

**Scenario:** two agents building a REST API.

1. Agent A derives `user schema`, `auth middleware`, `session store`, and records actions
   creating dependencies on them across a dozen interactions.
2. `user schema` accumulates inbound `Dependency` edges from concepts originating in six
   distinct interactions across most of the session's temporal extent. It progresses
   Candidate → Venerable → Canonical. `canonization_events` records each transition.
3. Agent B, working on a separate feature, calls `recall("update user schema")`. The context
   block returns `user schema [Entity, canonical]` with:
   `⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.`
   plus a conflict warning that Agent A wrote to it eleven seconds ago.
4. Agent B does not make the breaking change.
5. Split screen: Claude Code queries `canonization_events` through the CockroachDB MCP server
   and shows the promotion history that produced the warning.

Under three minutes. Exercises the bipartite graph, canonization, blast radius, conflict
detection, the vector index, and the MCP server. Answers "why a graph instead of top-k" in
one screen.

---

## 14. Build Plan

| Day | Work |
|-----|------|
| Sun Aug 9 | **Spike: `sqlx` against CockroachDB's `VECTOR` type.** Provision cluster via ccloud CLI, schema DDL, write and read back a 1024-dim embedding, run a vector-index query. Then repo, license, CI, `GraphStore` trait, `MemoryStore`. |
| Mon–Tue | *(Native Builder / DataHub submissions — Lambo paused)* |
| Wed Aug 12 | Graph core: nodes, edges, canonicalization pipeline, `derive()`, `record_action()`, cycle checks. |
| Thu Aug 13 | `CockroachStore` (`store-cockroach`) + `SqliteStore` (`store-sqlite`). Write-behind flush. `load_session()`. Structural queries. |
| Fri Aug 14 | Daemon: scoring, hot list, conflict detection, drift, GC. `recall()` all three phases. |
| Sat Aug 15 | Canonization, all three stages. Bedrock embedder behind `embed-bedrock` if authorized (else BGE). Multi-agent + reservations. |
| Sun Aug 16 | `lambo serve` loads Level B config, MCP tools. Demo app. Two-agent scenario scripted and reproducible (`--features demo`). |
| Mon Aug 17 | Video, README, architecture diagram. Buffer. |
| Tue Aug 18 | Submit before 5:00 pm ET (2:30 am IST Aug 19). |

**Decision gate — end of Sunday Aug 9.** `sqlx` has no native binding for Cockroach's
`VECTOR` type; it will most likely need encoding as a string literal or a custom `sqlx::Type`
impl. This is the single most likely place to lose half a day to something stupid, and it is
load-bearing for a required tool. If it isn't working by end of day Sunday, **fall back to
Python** — the graph logic is identical either way and the schedule cannot absorb a driver
fight in the middle of the week.

**If the schedule slips, cut in this order:** Lambda sweep → ccloud scripting → SQLite
adapter (`store-sqlite` feature stays optional) → reservations → drift detection.
**Never cut:** canonization, blast radius, the recall context format, Level B packaging
(feature gates + fail-closed selection — do not re-link every adapter into the default
binary).

---

## 15. Deferred to v0.7.0

Everything in this list is designed in v0.6.0 and deliberately not built here.

**Cut for scope:** PyO3 bindings and the Python wheel, C FFI, arena and generational
indexing, checkpoint/WAL durability, interaction compaction, forced GC, emergency eviction,
the control-channel protocol, session profiles (`creative`, `long_running`),
`CrossOccurrence` and `Fixes` edges, transitive synonyms and the merge token, `correct()`,
`merge_concepts()`, the callback pool and `PolicyAction`, structural centrality, the
embedding circuit breaker, custom stemmers and chunk splitters, access levels and floating
interaction nodes.

The Rust core is *not* cut — v0.1 is Rust. What's deferred is the distribution surface:
PyO3, maturin, and manylinux/macOS wheels are a v0.7.0 decision, made once there's demand
for embedding Lambo in a Python agent loop rather than connecting to it over MCP.

**Cut for architecture:** multi-writer coordination, and with it the divergence and merge
semantics that Dolt's three-way merge would need. Canonization status is *derived* state — if
it is ever merged across branches it must be recomputed, never resolved cell-wise.

**New in v0.7.0:** the `DoltStore` adapter, where a durable branch replaces the dry-run
traversal for blast radius and `dolt_history_*` replaces the `canonization_events` table;
a LoCoMo benchmark harness to test whether associative structure plus canonization actually
beats flat top-k on multi-hop; and the PyO3 distribution surface, if the MCP path turns out
not to be enough.

---

## Appendix: Name

**Lambo** — short for **Lambodaran** (ലംബോദരൻ), an epithet of Ganesha, the remover of
obstacles. A capacious memory that absorbs what it is offered, retains what proves
structurally load-bearing, and surfaces what is needed to navigate obstacles the agent
cannot see.
