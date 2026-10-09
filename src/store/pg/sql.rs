//! SQL composition for the Postgres-wire family (each adapter owns its SQL —
//! spec §3.2). Pure text and query builders, no I/O:
//!
//! * statement constants, byte-identical on every dialect;
//! * [`DialectSql`], the statements that differ only by a cast token, composed
//!   once per store from the [`Dialect`]'s tokens;
//! * the multi-row `QueryBuilder` composers the write path executes (upserts,
//!   write intents, read accesses) and the keyword-candidate statement.
//!
//! The over-merging rule from the family doc applies here: a statement that
//! differs between dialects by more than a token does not belong in this file.

use super::codec::{canonization_status_sql, concept_type_sql, edge_type_sql};
use super::Dialect;
use crate::store::batch::{AccessUpdate, ConceptRow};
use crate::types::{Edge, EmbeddingSource, Interaction};

/// Session row anchor + durable mutation-counter stamp (issue #17): `flush`
/// upserts a bare row per new session (created_at defaults to `now()`),
/// mirroring `MemoryStore::ensure_session`, and stamps the batch's absolute
/// `mutation_epoch` in the same statement — monotonic via `GREATEST`, so a
/// replayed batch converges to the same final state (the flush-replay
/// contract) and the counter commits atomically with the content it counts.
/// A batch of pure deletions resolves no session here; its epoch contribution
/// lands with the next batch that names one (the stamp is absolute, so the
/// counter only ever lags, never rewinds).
///
/// Issue #29: the same statement stamps GC's sweep mark (`last_gc_epoch`,
/// `last_gc_at`) with the store-side merge
/// [`crate::types::GcMark::apply_to_stored`]: a field-wise monotonic max,
/// except that a re-anchored mark (`$5`, `last_gc_at_reset`) at least as
/// current as the stored one (`EXCLUDED.last_gc_epoch >= sessions.last_gc_epoch`,
/// [`crate::types::GcMark::reset_is_current_for`]) replaces the stored
/// `last_gc_at` (never with NULL), so a future time left by a corrected
/// wall-clock jump cannot keep the time trigger off. A stale reset replayed
/// after a later sweep falls back to the max, so it cannot rewind that sweep's
/// time. `last_gc_epoch` is a max either way. `last_gc_at` is wrapped in `COALESCE` on both sides of the
/// `GREATEST` because the two dialects disagree about `GREATEST` over a NULL
/// argument; the wrapped form means "the later non-NULL value" on either.
pub(super) const UPSERT_SESSION_ROW_SQL: &str = r#"
INSERT INTO sessions (session_id, mutation_epoch, last_gc_epoch, last_gc_at)
VALUES ($1, $2, $3, $4)
ON CONFLICT (session_id) DO UPDATE SET
    mutation_epoch = GREATEST(sessions.mutation_epoch, EXCLUDED.mutation_epoch),
    last_gc_epoch = GREATEST(sessions.last_gc_epoch, EXCLUDED.last_gc_epoch),
    last_gc_at = CASE WHEN $5::BOOL AND EXCLUDED.last_gc_epoch >= sessions.last_gc_epoch
        THEN COALESCE(EXCLUDED.last_gc_at, sessions.last_gc_at)
        ELSE GREATEST(
            COALESCE(sessions.last_gc_at, EXCLUDED.last_gc_at),
            COALESCE(EXCLUDED.last_gc_at, sessions.last_gc_at)
        )
    END
"#;

/// Upserts are issued as **multi-row** statements (L82-1), so each is built as
/// `PREFIX` + a `VALUES` list of however many rows the plan put in the chunk +
/// `ON CONFLICT`. Splitting the statement in two named halves is what lets one
/// definition serve a 1-row seed and a 256-row flush chunk without the two
/// drifting apart. `sqlx::QueryBuilder` numbers the placeholders.
pub(super) const INSERT_INTERACTION_PREFIX_SQL: &str = r#"
INSERT INTO interactions (
    id, session_id, agent_id, prompt_text, previous_id, created_at, event_time
) "#;

pub(super) const ON_CONFLICT_INTERACTION_SQL: &str = r#"
ON CONFLICT (id) DO UPDATE SET
    session_id = EXCLUDED.session_id,
    agent_id = EXCLUDED.agent_id,
    prompt_text = EXCLUDED.prompt_text,
    previous_id = EXCLUDED.previous_id,
    created_at = EXCLUDED.created_at,
    event_time = EXCLUDED.event_time
"#;

/// 18 columns; `embedding` is bound as text and cast server-side with the
/// dialect's `VECTOR_CAST` (`$15::VECTOR` on Cockroach);
/// `chunk_group_id` (T2.5 sibling co-retrieval key) is the 16th, bound nullable;
/// `human_confirmed` (C2 solo-score input) is the 17th, bound as an INT count;
/// `embedding_source` (#22 supplied-vector provenance, compact JSON) is the
/// 18th, bound as nullable text. It is in the `DO UPDATE SET` list because it
/// describes `embedding`, which is too: a whole-record upsert that sets a
/// concept's source to `None` clears it. The embedding quarantine
/// ([`QUARANTINE_LEGACY_EMBEDDINGS_SQL`]) is not an upsert and keeps it.
///
/// **R2-1 — canonization columns are insert-only here.**
/// `canonization_status` / `blast_radius` / `last_demotion_time` are in the
/// INSERT column list (a brand-new row must carry them) but deliberately
/// **absent from the `DO UPDATE SET` list**: on an existing row the
/// canonization path (`UPDATE_CONCEPT_STATUS_SQL`) is their only writer.
/// A `Mutation::UpsertNode` carries a snapshot of the concept taken when it
/// was appended — a GC `bump_gc_survived` from before a hop would otherwise
/// overwrite the hop's effect from `EXCLUDED`, and the transition replaying
/// behind it in the same batch is a no-op by design (its audit row is already
/// recorded), so nothing repairs it. Full rationale on `Mutation::UpsertNode`.
pub(super) const INSERT_CONCEPT_PREFIX_SQL: &str = r#"
INSERT INTO concepts (
    id, session_id, content, canonical_key, concept_type,
    origin_interaction, origin_agent, created_at, access_count, last_accessed,
    gc_survived, canonization_status, blast_radius, last_demotion_time, embedding,
    chunk_group_id, human_confirmed, embedding_source
) "#;

pub(super) const ON_CONFLICT_CONCEPT_SQL: &str = r#"
ON CONFLICT (id) DO UPDATE SET
    session_id = EXCLUDED.session_id,
    content = EXCLUDED.content,
    canonical_key = EXCLUDED.canonical_key,
    concept_type = EXCLUDED.concept_type,
    origin_interaction = EXCLUDED.origin_interaction,
    origin_agent = EXCLUDED.origin_agent,
    created_at = EXCLUDED.created_at,
    access_count = EXCLUDED.access_count,
    last_accessed = EXCLUDED.last_accessed,
    gc_survived = EXCLUDED.gc_survived,
    embedding = EXCLUDED.embedding,
    chunk_group_id = EXCLUDED.chunk_group_id,
    human_confirmed = EXCLUDED.human_confirmed,
    embedding_source = EXCLUDED.embedding_source
"#;

/// Natural-key conflict target `(source, target, edge_type)` matches the graph tier's
/// `record_edge` dedup: a duplicate natural key **reinforces** (replaces) the row while
/// preserving nothing — the incoming record is authoritative (I2 convention: graph core
/// counts creation as the first write, `reinforcements = 1`; we store its values, never
/// the DDL default 0). Updating `id` on conflict mirrors MemoryStore's whole-record
/// replace; the graph never reuses an id with a different natural key.
pub(super) const INSERT_EDGE_PREFIX_SQL: &str = r#"
INSERT INTO edges (
    id, session_id, source, target, edge_type, weight, reinforcements,
    created_at, last_reinforced, event_time
) "#;

pub(super) const ON_CONFLICT_EDGE_SQL: &str = r#"
ON CONFLICT (source, target, edge_type) DO UPDATE SET
    id = EXCLUDED.id,
    session_id = EXCLUDED.session_id,
    weight = EXCLUDED.weight,
    reinforcements = EXCLUDED.reinforcements,
    created_at = EXCLUDED.created_at,
    last_reinforced = EXCLUDED.last_reinforced,
    event_time = EXCLUDED.event_time
"#;

pub(super) const DELETE_NODE_EDGES_SQL: &str = r#"
DELETE FROM edges WHERE source = $1 OR target = $1 OR id = $1
"#;

/// XP-8: persist a session's `root_goal` from the mutation path. Same column and
/// same JSONB cast `UPSERT_SESSION_SQL` (the `seed` path) uses, so a goal set
/// through a mutation and one seeded from a snapshot are indistinguishable on
/// reload. The bare-row upsert above has already created the row.
pub(super) const SET_ROOT_GOAL_SQL: &str = r#"
UPDATE sessions SET root_goal = $2::JSONB WHERE session_id = $1
"#;

/// Stamping a contract over an **unstamped** session NULLs its vectors: nothing
/// attested which space they were in, so they are unreadable by construction.
///
/// **Deliberate divergence from SQLite (F-R2-1), recorded here so it reads as a
/// decision and not an oversight.** SQLite's `set_embedding` (`sqlite/write_rows.rs`) widened the same
/// predicate to fire on any *width* change, not only over a NULL contract, because a
/// restamp there could leave earlier vectors under a width they no longer match. That
/// shape cannot arise on Cockroach: `concepts.embedding` is `VECTOR(1024)` in the DDL
/// (`migrations/cockroach/001_init.sql`), so every stored vector is exactly that wide
/// or NULL and no row can decode to an unexpected width — a restamp to some other
/// width instead makes the session refuse loudly at `check_embedding_dim` against the
/// DDL-parsed authority, before a single candidate row is read. SQLite's width-agnostic
/// `BLOB` has no such authority, which is why only it needs the wider rule. Widen this
/// statement too if Cockroach's width ever becomes configurable per deployment (B2's
/// Postgres split is where that would land).
///
/// **The quarantine nulls `embedding` only; `embedding_source` is kept** (#22
/// review L1, decided). A quarantined image concept is still an image
/// concept: keeping its source stops a later `re-embed --missing-only` from
/// giving it a vector of its caption. A concept can therefore load with a
/// source and no vector, and PR 3's re-embed treats that as "image vector
/// missing", not as a text concept.
pub(super) const QUARANTINE_LEGACY_EMBEDDINGS_SQL: &str = r#"
UPDATE concepts SET embedding = NULL
WHERE session_id = $1 AND EXISTS (
    SELECT 1 FROM sessions
    WHERE session_id = $1 AND embedding_kind IS NULL AND embedding_dim IS NULL
)
"#;

pub(super) const DELETE_NODE_CONCEPTS_SQL: &str = r#"
DELETE FROM concepts WHERE id = $1
"#;

pub(super) const DELETE_NODE_INTERACTIONS_SQL: &str = r#"
DELETE FROM interactions WHERE id = $1
"#;

pub(super) const DELETE_EDGE_SQL: &str = r#"
DELETE FROM edges WHERE id = $1
"#;

/// Owning sessions of the rows a flush's deletes will remove, so the fencing
/// gate covers a delete-only batch. `$1` is the `DeleteNode` ids, `$2` the
/// `DeleteEdge` ids. Mirrors the delete statements above: a node delete
/// removes the interaction or concept with that id and every edge incident to
/// it (`DELETE_NODE_EDGES_SQL`), an edge delete removes the edge row.
pub(super) const DELETED_ROW_SESSIONS_SQL: &str = r#"
SELECT session_id FROM interactions WHERE id = ANY($1)
UNION SELECT session_id FROM concepts WHERE id = ANY($1)
UNION SELECT session_id FROM edges
    WHERE id = ANY($2) OR id = ANY($1) OR source = ANY($1) OR target = ANY($1)
"#;

/// The fencing gate's read of a session's lease token. `FOR SHARE` keeps the
/// lease row locked until the surrounding transaction ends, so a takeover or
/// renewal (`INSERT ... ON CONFLICT DO UPDATE` takes a conflicting row lock)
/// waits for our commit instead of slipping between the check and the commit
/// under PostgreSQL's READ COMMITTED. Share, not exclusive: concurrent flushes
/// of one session do not serialise on each other, and the only writers of the
/// row (acquire, renew, release) are the ones that must wait. CockroachDB runs
/// SERIALIZABLE and already aborts the loser; the clause is accepted there and
/// harmless, so the statement stays shared rather than dialect-specific.
///
/// The holder comes back with the token (second column, so a scalar read of
/// the first still sees the token) so the fence can refuse an erasure
/// tombstone whatever token a write presents (#23 review H1).
pub(super) const LEASE_TOKEN_FOR_SHARE_SQL: &str =
    "SELECT current_token, holder FROM session_leases WHERE session_id = $1 FOR SHARE";

// ---------------------------------------------------------------------------
// Session erasure (#23). The transaction is `persistence.rs`'s `erase`.
// ---------------------------------------------------------------------------

/// The erase transaction's first lock: the session's `sessions` row, taken
/// before the lease row (#23 review L1). A flush stamps (and so row-locks)
/// `sessions` first and reads its lease fence second; an erase that locked the
/// lease first and deleted `sessions` last took the same two rows in the
/// opposite order, so a zombie flush and an erase could deadlock (40P01 on
/// Postgres, retried by `tx_retry` after `deadlock_timeout`). Taking
/// `sessions` first gives both transactions one order. No row (a never-written
/// session) locks nothing, and there is then no cycle to break: a flush that
/// inserts the row concurrently blocks on the lease row instead.
pub(super) const ERASE_SESSION_ROW_FOR_UPDATE_SQL: &str =
    "SELECT 1 FROM sessions WHERE session_id = $1 FOR UPDATE";

/// The erase transaction's lease read. `FOR UPDATE` takes the row lock every
/// flush's `FOR SHARE` fence read conflicts with, so a flush already past its
/// fence commits before the erase reads on, and one arriving later waits for
/// the erase and then reads the tombstone. `live` is on the cluster clock.
pub(super) const ERASE_LEASE_FOR_UPDATE_SQL: &str = "SELECT holder, expires_at > now() \
     FROM session_leases WHERE session_id = $1 FOR UPDATE";

/// The erasure tombstone over the session's lease row (see `store::erase`).
/// Guarded like an acquire: it fires on no row, an expired row, an earlier
/// tombstone (`$2`) or the eraser's own lease (`$4`), bumping the token unless
/// the row already is a tombstone. An empty `RETURNING` means a live lease
/// belongs to someone else. `$3` is the year-9999 expiry.
pub(super) const ERASE_TOMBSTONE_SQL: &str = "\
    INSERT INTO session_leases \
        (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
    VALUES ($1, $2, now(), $3, 1, NULL) \
    ON CONFLICT (session_id) DO UPDATE SET \
        holder = excluded.holder, \
        acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                           THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
        expires_at = excluded.expires_at, \
        current_token = CASE WHEN session_leases.holder = excluded.holder \
                             THEN session_leases.current_token \
                             ELSE session_leases.current_token + 1 END, \
        endpoint = NULL \
    WHERE session_leases.expires_at <= now() \
       OR session_leases.holder = excluded.holder \
       OR session_leases.holder = $4 \
    RETURNING current_token";

/// Concepts of the session that carry an embedding, counted before they go.
pub(super) const COUNT_SESSION_VECTORS_SQL: &str =
    "SELECT COUNT(*) FROM concepts WHERE session_id = $1 AND embedding IS NOT NULL";

/// Edges in **other** sessions incident to the erased session's nodes (#23
/// review L4). Node ids are global and edges session-scoped, so an edge in
/// session B can point at a node of session A; `DeleteNode` removes such
/// edges with the node, and erasure does the same so no row keeps a deleted
/// account's node id. Run with the `edges` step, before `concepts` and
/// `interactions` go (the subqueries read them); counted in `edges`.
pub(super) const ERASE_CROSS_SESSION_EDGES_SQL: &str = "\
    DELETE FROM edges WHERE session_id <> $1 AND ( \
        source IN (SELECT id FROM concepts WHERE session_id = $1) \
     OR source IN (SELECT id FROM interactions WHERE session_id = $1) \
     OR target IN (SELECT id FROM concepts WHERE session_id = $1) \
     OR target IN (SELECT id FROM interactions WHERE session_id = $1))";

/// Every session-keyed table erasure empties, with its DELETE, in dependency
/// order: referencing rows first (`write_intents`, `synonyms`, `edges` and
/// `concepts` reference `sessions`; `concepts` references `interactions`;
/// `interactions` references itself, which one statement over the whole
/// session satisfies, both engines checking at end of statement), `sessions`
/// last. `session_leases` is absent on purpose: its row becomes the tombstone.
/// `erase_covers_every_table_in_both_ddls` diffs this list against the shipped
/// Postgres and Cockroach migrations.
pub(super) const ERASE_STATEMENTS: &[(&str, &str)] = &[
    (
        "write_intents",
        "DELETE FROM write_intents WHERE session_id = $1",
    ),
    (
        "canonization_events",
        "DELETE FROM canonization_events WHERE session_id = $1",
    ),
    (
        "reservations",
        "DELETE FROM reservations WHERE session_id = $1",
    ),
    ("synonyms", "DELETE FROM synonyms WHERE session_id = $1"),
    ("edges", "DELETE FROM edges WHERE session_id = $1"),
    ("concepts", "DELETE FROM concepts WHERE session_id = $1"),
    (
        "interactions",
        "DELETE FROM interactions WHERE session_id = $1",
    ),
    (
        "session_stats",
        "DELETE FROM session_stats WHERE session_id = $1",
    ),
    (
        "lease_refusals",
        "DELETE FROM lease_refusals WHERE session_id = $1",
    ),
    ("sessions", "DELETE FROM sessions WHERE session_id = $1"),
];

/// Canonization transition: update the concept (parity with MemoryStore's
/// `CanonizationTransition` application) and append the audit row. The event insert is
/// `ON CONFLICT (id) DO NOTHING` so a retried flush (same batch, already-committed
/// response lost) cannot duplicate the demo's on-screen artifact.
///
/// COH-3: `last_demotion_time = COALESCE($5, last_demotion_time)` — a demotion
/// event (which always carries `Some`) stamps the concept; non-demotion events
/// (`None`) leave a previously demoted value untouched (spec §10).
pub(super) const UPDATE_CONCEPT_STATUS_SQL: &str = r#"
UPDATE concepts
SET canonization_status = $2, blast_radius = $3,
    last_demotion_time = COALESCE($5, last_demotion_time)
WHERE id = $1 AND session_id = $4
"#;

pub(super) const INSERT_CANONIZATION_EVENT_SQL: &str = r#"
INSERT INTO canonization_events (
    id, session_id, node_id, from_status, to_status, blast_radius,
    last_demotion_time, occurred_at
) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (id) DO NOTHING
"#;

// Fixtures `seed` and the Cockroach SQL-shape tests. Not part of the B1
// postgres stub's surface: compiling them under store-postgres-only would
// be dead_code (no reader).
#[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
pub(super) const UPSERT_SYNONYM_SQL: &str = r#"
INSERT INTO synonyms (session_id, source_key, canonical_key)
VALUES ($1, $2, $3)
ON CONFLICT (session_id, source_key) DO UPDATE SET
    canonical_key = EXCLUDED.canonical_key
"#;

#[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
pub(super) const UPSERT_RESERVATION_SQL: &str = r#"
INSERT INTO reservations (session_id, node_id, agent_id, expires_at)
VALUES ($1, $2, $3, $4)
ON CONFLICT (session_id, node_id) DO UPDATE SET
    agent_id = EXCLUDED.agent_id,
    expires_at = EXCLUDED.expires_at
"#;

pub(super) const SELECT_SYNONYMS_SQL: &str = r#"
SELECT session_id, source_key, canonical_key
FROM synonyms
WHERE session_id = $1
"#;

/// Blast radius (spec §4.1 + errata, `MemoryStore`-equivalent): count concepts that have
/// at least one **aged** inbound structural edge (`Dependency`/`Causal`/`Hierarchical`)
/// from `node` and no aged inbound structural edge from any other concept. Provenance
/// `Derives`/`Temporal` edges must not un-orphan (errata; T1.4). `src.session_id = $1`
/// keeps the source-concept check session-scoped (MemoryStore's `concept_ids`), and
/// `c.id <> $2` excludes a hypothetical self-loop, matching MemoryStore's skip.
/// Placeholders: `$1` session, `$2` node, `$3` cutoff timestamp (Rust-computed).
pub(super) const BLAST_RADIUS_SQL: &str = r#"
SELECT count(*) AS n
FROM concepts c
WHERE c.session_id = $1
  AND c.id <> $2
  AND EXISTS (
      SELECT 1 FROM edges e
      JOIN concepts src ON src.id = e.source AND src.session_id = $1
      WHERE e.target = c.id AND e.source = $2
        AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND COALESCE(e.event_time, e.created_at) <= $3
  )
  AND NOT EXISTS (
      SELECT 1 FROM edges e2
      JOIN concepts src2 ON src2.id = e2.source AND src2.session_id = $1
      WHERE e2.target = c.id AND e2.source <> $2
        AND e2.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
        AND COALESCE(e2.event_time, e2.created_at) <= $3
  )
"#;

/// Interaction span + temporal coverage (spec §4.1, `MemoryStore`-equivalent): distinct
/// origin interactions of concepts reachable via aged structural inbound edges, and the
/// share of the session's temporal extent those interactions cover. Filters BOTH the
/// edge and the interaction age (MemoryStore parity — see module doc). The `CASE` keeps
/// the coverage `0.0` when no spans match (a `NULL` epoch-arithmetic result would
/// otherwise surface as a decode error). The extent is never `NULL` while the span is
/// non-empty (the span's interactions belong to the same session), so the `ELSE` arm
/// covers exactly one case: a non-empty span over a **single-point session extent**
/// (extent <= 0) — that interaction spans the whole session, so coverage is `1.0`
/// (F1: canonization Stage 2 parity with MemoryStore in short sessions).
///
/// **Session scope (F5).** `i.session_id = $1` is not redundant with
/// `e.session_id = $1`: `concepts.origin_interaction` is a **global** FK, so a
/// concept in session S may legally point at an interaction in session S′.
/// Without the filter the span counted those foreign interactions — inflating
/// `distinct` against a session `MemoryStore` never sees, while the extent CTE
/// stayed session-filtered. The `least`/`greatest` clamp is the same guard
/// `MemoryStore` and SQLite apply in Rust (`clamp(0.0, 1.0)`): a span whose
/// endpoints fall outside the session extent must not report a ratio above 1.0.
/// Placeholders: `$1` session, `$2` node, `$3` cutoff timestamp.
pub(super) const INTERACTION_SPAN_SQL: &str = r#"
WITH span AS (
    SELECT DISTINCT i.id AS iid, COALESCE(i.event_time, i.created_at) AS ts
    FROM edges e
    JOIN concepts src ON src.id = e.source AND src.session_id = $1
    JOIN interactions i ON i.id = src.origin_interaction AND i.session_id = $1
    WHERE e.target = $2
      AND e.session_id = $1
      AND e.edge_type IN ('Dependency', 'Causal', 'Hierarchical')
      AND COALESCE(e.event_time, e.created_at) <= $3
      AND COALESCE(i.event_time, i.created_at) <= $3
),
extent AS (
    SELECT min(COALESCE(event_time, created_at)) AS lo,
           max(COALESCE(event_time, created_at)) AS hi
    FROM interactions WHERE session_id = $1
)
SELECT
    (SELECT count(*) FROM span) AS distinct_count,
    -- B0-R4-1: the outer cast is load-bearing, not cosmetic. PostgreSQL 14
    -- changed `extract` to return `numeric` rather than `double precision`,
    -- and a bare `0.0` / `1.0` literal is `numeric` too -- so WITHOUT this
    -- cast every arm of the CASE is numeric, `try_get::<f64>("coverage")`
    -- fails unconditionally with "Rust type `f64` (as SQL type `FLOAT8`) is
    -- not compatible with SQL type `NUMERIC`", and the whole canonization
    -- cycle aborts at the Stage-2 gate that reads it. Stage 1's transitions
    -- commit first, so the visible symptom is a graph that reaches Candidate
    -- and never moves again -- not an obvious type error. SQLite's affinity
    -- accepts the same value as a float, so only a Postgres-backed test sees
    -- this. `InteractionSpan::coverage` is an f64 ratio, not money: the
    -- contract belongs here at the boundary, not in a Decimal conversion.
    CAST(
        CASE
            WHEN (SELECT count(*) FROM span) = 0 THEN 0.0
            WHEN extract(epoch FROM (extent.hi - extent.lo)) > 0
                THEN least(1.0, greatest(0.0,
                     extract(epoch FROM ((SELECT max(ts) FROM span) - (SELECT min(ts) FROM span)))
                     / extract(epoch FROM (extent.hi - extent.lo))))
            -- F1: non-empty span over a single-point session extent covers the
            -- whole session -> 1.0 (the count = 0 arm above handles empty spans).
            ELSE 1.0
        END
    AS double precision) AS coverage
FROM extent
"#;

/// Every statement whose only dialect variation is a cast token or the distance
/// operator, built once per store from [`Dialect::STRING_CAST`],
/// [`Dialect::VECTOR_CAST`] and [`Dialect::DISTANCE_OP`].
///
/// **Built once, not per call.** `vector_candidates` re-issues its statement
/// inside a grow-and-retry loop and `load_session` issues six of these in one
/// transaction, so composing them on every query would put an allocation on
/// the hot recall path that the constants it replaces did not have.
///
/// **The SQL text is written exactly once**, here, so two dialects cannot drift
/// by anything except the tokens they substitute. That is the whole point of
/// the extraction: a statement that differs by *more* than a token does not
/// belong in this struct, and does not belong in [`PgStore`](super::PgStore) either.
pub(super) struct DialectSql {
    /// This dialect's cast to its dense-vector type, carried as a token rather
    /// than baked into a statement: it rides the concept upsert's embedding
    /// placeholder, which `sqlx::QueryBuilder` numbers at build time.
    pub(super) vector_cast: &'static str,

    /// GLOBAL vector top-k — deliberately omits any session predicate so the planner
    /// uses `concepts@concepts_embedding_idx` (DECISION D1). `session_id` is selected so
    /// the Rust side can drop foreign-session rows. Ordering is distance ascending,
    /// i.e. similarity (score) descending. The adapter requests `k + 1`:
    /// a lookahead tied with the kth distance triggers the exact, UUID-ordered
    /// session fallback, while an untied boundary remains on this index-friendly path.
    pub(super) vector_candidates: String,

    /// Correctness fallback when foreign-session rows crowd the caller out of the
    /// capped global index query. This deliberately prioritizes exact session-local
    /// top-k over index use; it runs only after the bounded fast path is exhausted.
    pub(super) session_vector_candidates: String,

    /// Full-snapshot sessions upsert (fixtures `seed` path): root_goal JSONB, created_at,
    /// closed_at, and the `EmbeddingContract` columns (STORE-1). `COALESCE($3, now())`
    /// keeps the NOT NULL default when a snapshot omits it.
    #[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
    pub(super) upsert_session: String,

    /// `Mutation::SetEmbedding`'s session-column write.
    pub(super) set_embedding: String,

    /// The session row `load_session` and the checked vector read both start from.
    pub(super) select_session: String,
    pub(super) select_interactions: String,
    pub(super) select_concepts: String,
    pub(super) select_edges: String,
    pub(super) select_canonization_events: String,
    pub(super) select_reservations: String,
}

impl DialectSql {
    pub(super) fn for_dialect<D: Dialect>() -> Self {
        let s = D::STRING_CAST;
        let v = D::VECTOR_CAST;
        let op = D::DISTANCE_OP;
        Self {
            vector_cast: v,
            vector_candidates: format!(
                r#"
SELECT id{s} AS id, session_id{s} AS session_id,
       canonical_key{s} AS canonical_key,
       embedding {op} $1{v} AS dist
FROM concepts
WHERE embedding IS NOT NULL
ORDER BY dist ASC
LIMIT $2
"#
            ),
            session_vector_candidates: format!(
                r#"
SELECT id{s} AS id, canonical_key{s} AS canonical_key,
       embedding {op} $1{v} AS dist
FROM concepts
WHERE session_id = $2 AND embedding IS NOT NULL
ORDER BY dist ASC, id ASC
LIMIT $3
"#
            ),
            #[cfg(any(feature = "fixtures", all(test, feature = "store-cockroach")))]
            upsert_session: format!(
                r#"
INSERT INTO sessions (
    session_id, root_goal, created_at, closed_at,
    embedding_kind, embedding_model, embedding_dim, mutation_epoch,
    last_gc_epoch, last_gc_at
) VALUES ($1, $2::JSONB, COALESCE($3, now()), $4, $5{s}, $6{s}, $7::INT, $8::INT, $9::BIGINT, $10)
ON CONFLICT (session_id) DO UPDATE SET
    root_goal = EXCLUDED.root_goal,
    created_at = EXCLUDED.created_at,
    closed_at = EXCLUDED.closed_at,
    embedding_kind = EXCLUDED.embedding_kind,
    embedding_model = EXCLUDED.embedding_model,
    embedding_dim = EXCLUDED.embedding_dim,
    mutation_epoch = EXCLUDED.mutation_epoch,
    last_gc_epoch = EXCLUDED.last_gc_epoch,
    last_gc_at = EXCLUDED.last_gc_at
"#
            ),
            set_embedding: format!(
                r#"
UPDATE sessions
SET embedding_kind = $2{s},
    embedding_model = $3{s},
    embedding_dim = $4::INT
WHERE session_id = $1
"#
            ),
            select_session: format!(
                r#"
SELECT root_goal{s} AS root_goal, created_at, closed_at,
       embedding_kind, embedding_model, embedding_dim, mutation_epoch,
       last_gc_epoch, last_gc_at
FROM sessions
WHERE session_id = $1
"#
            ),
            select_interactions: format!(
                r#"
SELECT id{s} AS id, session_id, agent_id, prompt_text,
       previous_id{s} AS previous_id, created_at, event_time
FROM interactions
WHERE session_id = $1
ORDER BY created_at, id
"#
            ),
            select_concepts: format!(
                r#"
SELECT id{s} AS id, session_id, content, canonical_key, concept_type,
       origin_interaction{s} AS origin_interaction, origin_agent, created_at,
       access_count, last_accessed, gc_survived, canonization_status, blast_radius,
       last_demotion_time, embedding{s} AS embedding, chunk_group_id, human_confirmed,
       embedding_source
FROM concepts
WHERE session_id = $1
ORDER BY id
"#
            ),
            select_edges: format!(
                r#"
SELECT id{s} AS id, session_id, source{s} AS source,
       target{s} AS target, edge_type, weight, reinforcements,
       created_at, last_reinforced, event_time
FROM edges
WHERE session_id = $1
ORDER BY id
"#
            ),
            select_canonization_events: format!(
                r#"
SELECT id{s} AS id, session_id, node_id{s} AS node_id,
       from_status, to_status, blast_radius, last_demotion_time, occurred_at
FROM canonization_events
WHERE session_id = $1
ORDER BY occurred_at, id
"#
            ),
            select_reservations: format!(
                r#"
SELECT session_id, node_id{s} AS node_id, agent_id, expires_at
FROM reservations
WHERE session_id = $1
"#
            ),
        }
    }
}

/// Build the keyword-candidate SQL for `n` tokens: `$1` = session, `$2..$n+1` = tokens
/// (each bound once, used twice — content and canonical_key). `strpos(lower(col), $k) > 0`
/// is exact-substring matching with no `LIKE` wildcard semantics (Rust `contains`
/// parity). Full scan is acceptable here — the RAM inverted index is the real path.
pub(super) fn keyword_candidates_sql<D: Dialect>(n_tokens: usize) -> String {
    debug_assert!(n_tokens > 0);
    let mut sql = String::with_capacity(64 + n_tokens * 96);
    let s = D::STRING_CAST;
    sql.push_str(&format!(
        "SELECT id{s} AS id, content, canonical_key FROM concepts WHERE session_id = $1 AND ("
    ));
    for i in 0..n_tokens {
        if i > 0 {
            sql.push_str(" OR ");
        }
        let n = i + 2;
        sql.push_str(&format!(
            "strpos(lower(content), ${n}) > 0 OR strpos(lower(canonical_key), ${n}) > 0"
        ));
    }
    sql.push(')');
    sql
}

/// The statement [`bulk_upsert_interactions`](super::write_rows::bulk_upsert_interactions) runs, built but not executed.
///
/// Separate from the execution so the generated SQL — the one part of this
/// change no local test can reach through a cluster — is inspectable by
/// `sql_shape_is_a_multi_row_upsert`.
pub(super) fn interaction_upsert_query<'a>(
    rows: &'a [&'a Interaction],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_INTERACTION_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, i| {
        b.push_bind(i.id.0)
            .push_bind(i.session_id.0.as_str())
            .push_bind(i.agent_id.0.as_str())
            .push_bind(i.prompt_text.as_deref())
            .push_bind(i.previous_id.map(|n| n.0))
            .push_bind(i.created_at)
            .push_bind(i.event_time);
    });
    qb.push(ON_CONFLICT_INTERACTION_SQL);
    qb
}

/// The statement [`bulk_upsert_concepts`](super::write_rows::bulk_upsert_concepts) runs, built but not executed.
pub(super) fn concept_upsert_query<'a>(
    rows: &'a [ConceptRow<'a>],
    embeddings: &'a [Option<String>],
    vector_cast: &'static str,
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_CONCEPT_PREFIX_SQL);
    qb.push_values(
        rows.iter().zip(embeddings.iter()),
        |mut b, (r, embedding)| {
            let c = r.concept;
            b.push_bind(c.id.0)
                .push_bind(c.session_id.0.as_str())
                .push_bind(c.content.as_str())
                .push_bind(c.canonical_key.as_str())
                .push_bind(concept_type_sql(c.concept_type))
                .push_bind(c.origin_interaction.0)
                .push_bind(c.origin_agent.0.as_str())
                .push_bind(c.created_at)
                .push_bind(c.access_count)
                .push_bind(c.last_accessed)
                .push_bind(c.gc_survived)
                .push_bind(canonization_status_sql(r.canonization.status))
                .push_bind(r.canonization.blast_radius)
                .push_bind(r.canonization.last_demotion_time)
                .push_bind(embedding.as_deref())
                // The dense-vector column takes a text literal, so the cast is
                // part of the value expression — it must ride with this
                // placeholder, not with the separator.
                .push_unseparated(vector_cast)
                .push_bind(c.chunk_group_id.as_deref())
                .push_bind(c.human_confirmed)
                // Owned: `to_column` encodes and cannot fail, so it can run
                // inside this closure, unlike the vector encode. The digest
                // was checked before the query was built
                // (`EmbeddingSource::check_writable`).
                .push_bind(c.embedding_source.as_ref().map(EmbeddingSource::to_column));
        },
    );
    qb.push(ON_CONFLICT_CONCEPT_SQL);
    qb
}

/// The statement [`bulk_upsert_edges`](super::write_rows::bulk_upsert_edges) runs, built but not executed.
pub(super) fn edge_upsert_query<'a>(
    rows: &'a [&'a Edge],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_EDGE_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, e| {
        b.push_bind(e.id.0)
            .push_bind(e.session_id.0.as_str())
            .push_bind(e.source.0)
            .push_bind(e.target.0)
            .push_bind(edge_type_sql(e.edge_type))
            .push_bind(e.weight)
            .push_bind(e.reinforcements)
            .push_bind(e.created_at)
            .push_bind(e.last_reinforced)
            .push_bind(e.event_time);
    });
    qb.push(ON_CONFLICT_EDGE_SQL);
    qb
}

pub(super) const INSERT_WRITE_INTENT_PREFIX_SQL: &str = r#"
INSERT INTO write_intents (
    session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload,
    created_at, consumed_at, outcome_tag, outcome_summary
) "#;

pub(super) const ON_CONFLICT_WRITE_INTENT_SQL: &str = r#"
ON CONFLICT (session_id, receipt) DO UPDATE SET
    agent = EXCLUDED.agent,
    interaction_id = EXCLUDED.interaction_id,
    lane_seq = EXCLUDED.lane_seq,
    issued_ms = EXCLUDED.issued_ms,
    payload = EXCLUDED.payload,
    created_at = EXCLUDED.created_at,
    consumed_at = EXCLUDED.consumed_at,
    outcome_tag = EXCLUDED.outcome_tag,
    outcome_summary = EXCLUDED.outcome_summary
"#;

/// The statement [`bulk_put_write_intents`](super::write_rows::bulk_put_write_intents) runs, built but not executed.
pub(super) fn put_write_intents_upsert<'a>(
    intents: &'a [&'a crate::types::WriteIntent],
    payloads: &'a [String],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(INSERT_WRITE_INTENT_PREFIX_SQL);
    qb.push_values(intents.iter().zip(payloads), |mut b, (i, payload)| {
        b.push_bind(i.session_id.0.as_str())
            .push_bind(&i.receipt)
            .push_bind(i.agent.0.as_str())
            .push_bind(i.interaction.0)
            .push_bind(i64::try_from(i.lane_seq).unwrap_or(i64::MAX))
            .push_bind(i.issued_ms)
            .push_bind(payload)
            .push_bind(i.created_at)
            .push_bind(i.outcome.as_ref().map(|o| o.consumed_at))
            .push_bind(i.outcome.as_ref().map(|o| o.tag.clone()))
            .push_bind(i.outcome.as_ref().map(|o| o.summary.clone()));
    });
    qb.push(ON_CONFLICT_WRITE_INTENT_SQL);
    qb
}

// `sqlx::QueryBuilder::push_values` emits the `VALUES` keyword itself, so the
// prefix ends after `FROM (` and the suffix begins after the closing paren.
pub(super) const CONSUME_WRITE_INTENT_UPDATE_PREFIX_SQL: &str = r#"
UPDATE write_intents SET
    consumed_at = v.consumed_at, outcome_tag = v.outcome_tag, outcome_summary = v.outcome_summary
    FROM ("#;

pub(super) const CONSUME_WRITE_INTENT_UPDATE_SUFFIX_SQL: &str = r#") AS v(
    session_id, receipt, consumed_at, outcome_tag, outcome_summary
) WHERE write_intents.session_id = v.session_id AND write_intents.receipt = v.receipt"#;

/// The statement [`bulk_consume_write_intents`](super::write_rows::bulk_consume_write_intents) runs, built but not executed.
pub(super) fn consume_write_intents_update<'a>(
    consumes: &'a [(
        &'a crate::types::SessionId,
        &'a str,
        &'a crate::types::WriteIntentOutcome,
    )],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(CONSUME_WRITE_INTENT_UPDATE_PREFIX_SQL);
    qb.push_values(consumes.iter(), |mut b, (sid, receipt, outcome)| {
        b.push_bind(sid.0.as_str())
            .push_bind(receipt)
            .push_bind(outcome.consumed_at)
            .push_bind(&outcome.tag)
            .push_bind(&outcome.summary);
    });
    qb.push(CONSUME_WRITE_INTENT_UPDATE_SUFFIX_SQL);
    qb
}

// Issue #30: the batched read-access update. Byte-identical on both dialects —
// every bind is typed by the driver (UUID, text, INT8, TIMESTAMPTZ), so no cast
// token is needed and it lives here rather than in `DialectSql`. Same
// `UPDATE … FROM (VALUES …) AS v(…)` shape as the write-intent consume above;
// `push_values` emits the `VALUES` keyword itself.
//
// Monotonic on both columns, so replaying a retained batch, or running after a
// concept upsert in the same flush, can never lower what is stored. `GREATEST`
// ignores NULLs on PostgreSQL; the `COALESCE` makes the never-read row
// explicit rather than relying on that per engine.
pub(super) const UPDATE_ACCESSES_PREFIX_SQL: &str = r#"
UPDATE concepts SET
    access_count = GREATEST(concepts.access_count, v.access_count),
    last_accessed = GREATEST(COALESCE(concepts.last_accessed, v.last_accessed), v.last_accessed)
    FROM ("#;

pub(super) const UPDATE_ACCESSES_SUFFIX_SQL: &str = r#") AS v(
    id, session_id, access_count, last_accessed
) WHERE concepts.id = v.id AND concepts.session_id = v.session_id"#;

/// The statement [`bulk_update_accesses`](super::write_rows::bulk_update_accesses) runs, built but not executed.
pub(super) fn access_update_query<'a>(
    rows: &'a [AccessUpdate<'a>],
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(UPDATE_ACCESSES_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, r| {
        b.push_bind(r.id.0)
            .push_bind(r.session_id.0.as_str())
            // INT8 on the wire: the column is BIGINT (PostgreSQL) / INT8
            // (CockroachDB), and `GREATEST` wants one type on both sides.
            .push_bind(i64::from(r.access_count))
            .push_bind(r.last_accessed);
    });
    qb.push(UPDATE_ACCESSES_SUFFIX_SQL);
    qb
}
