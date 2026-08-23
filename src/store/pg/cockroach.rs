//! T3.2 — `CockroachStore`: durable [`crate::store::GraphStore`] over `sqlx::PgPool`
//! (spec §3.2/§3.3, §4).
//!
//! Feature: `store-cockroach` (pulls `sqlx` + the `postgres` driver). Registered in
//! [`crate::store::build_store`] for [`crate::store::StoreKind::Cockroach`]. All SQL is runtime
//! `sqlx::query` (spec §3.2 — no compile-time macros).
//!
//! **What this file holds after B0's extraction.** `CockroachStore` is now the
//! type alias [`CockroachStore`] for `PgStore<CockroachDialect>`: the machinery
//! and the statements live once in [`super`], and this file holds the dialect,
//! the embedded `001_init.sql`, the width-from-DDL authority, and the
//! Cockroach-specific tests. The design log below is unchanged and still
//! authoritative: every decision it records is a decision about behaviour B0
//! moved without altering, so it stays written where it was reviewed rather
//! than being paraphrased into a new file. Where it says "this module", read
//! "this adapter"; the code it describes is in `pg/mod.rs`.
//!
//! # Design decisions (see PHASE-3-stores.md Handoff Log T3.2)
//!
//! - **Batch replay order (flush).** Mutations are replayed **in submission order**
//!   inside ONE transaction — never re-grouped by kind. The graph tier guarantees
//!   spec §2.4's grouping (nodes → edges → deletions → transitions) *within a single
//!   logical write*, but the drained log is chronological *across* writes, and a node
//!   upsert may legally follow a `DeleteNode` of the same id in one batch
//!   (create → delete → create within one flush interval). `src/graph/mod.rs` (T2.1
//!   M2 review close) is explicit: *"Store adapters (T3.4+) MUST replay batches in
//!   order and MUST NOT re-sort them."* `MemoryStore` does the same.
//! - **Concept upsert target is `ON CONFLICT (id)`** — not the canonical-key partial
//!   index. Rationale: a re-upsert of an existing concept (id present) must replace the
//!   whole row, and the id is the only *total* uniqueness constraint. The partial
//!   `concepts_key_non_obs_idx` (`(session_id, canonical_key) WHERE concept_type <>
//!   'Observation'`, spec §4 errata / muse-spark M1-M2) can only reject an INSERT whose
//!   key collides with a **non-Observation** concept — exactly the case the RAM graph
//!   already rejects as an invariant (`Graph::insert_concept`). A legal demote writes an
//!   **Observation** sharing a key; the partial index excludes it, so the INSERT
//!   succeeds. No `ON CONFLICT (session_id, canonical_key) WHERE ...` spelling is
//!   needed and none is used (that target would not cover id-based re-upserts).
//! - **DeleteNode cleans incident edges explicitly.** `edges.source`/`target` carry no
//!   `REFERENCES` (spec §4) — Cockroach enforces only declared FKs, so a deleted node
//!   would leave dangling edges unless removed. We delete edges where
//!   `source = $1 OR target = $1 OR id = $1` (MemoryStore parity). `canonization_events`
//!   is append-only (the demo artifact) and is *not* cleaned. Interactions are
//!   append-only in v0.1 (graph contract) so `DeleteNode` on an interaction id cannot
//!   legally occur; if it ever does, the enforced FK from
//!   `concepts.origin_interaction`/`interactions.previous_id` fails the delete loudly
//!   rather than corrupting the graph.
//! - **VECTOR encode/decode (T0.3 spike, Attempt A).** Bind the embedding as a text
//!   literal and cast server-side (`$n::VECTOR`); read back via `embedding::STRING` and
//!   parse. Text form is `[x,y,z]` with Rust's shortest-round-trip `f32` `Display`
//!   (spike-verified exact at eps=1e-4 over 1024 dims). Non-finite elements are
//!   rejected at encode time.
//! - **vector_candidates score = cosine similarity** derived from the L2 distance the
//!   `<->` operator returns: `1 - d²/2`, clamped to [-1, 1]. This is the metric
//!   `semantic_match_threshold` (spec §7.1 step 6, 0.85) is written against; a raw
//!   distance would be backwards. Exact only for unit-normalized embeddings (the
//!   pipeline normalizes — Titan `normalize=true`, spike normalizes).
//! - **§4.1 queries bind a Rust-computed cutoff, not an `INTERVAL` literal.** T3.3/T3.6
//!   contract: SQLite has no `INTERVAL`, so both dialects compute
//!   `now - min_age` in Rust and bind it, keeping the two queries twin-shaped. The
//!   interaction_span query additionally filters `edges.created_at` (not just
//!   `i.created_at` as the spec's literal SQL shows) to match `MemoryStore`'s naive
//!   answer — T3.6's three-way agreement test defines agreement against MemoryStore.
//!   Full semantics (locked by the T3.6 matrix on BOTH fixtures × every node ×
//!   min-age {0, 3600s} × MemoryStore equality, plus the errata/aged-edge probes):
//!   (1) **errata exclusions (2026-08-11 / T1.4)** — only concept-sourced
//!   `Dependency`/`Causal`/`Hierarchical` count; provenance `Derives`
//!   (interaction → concept, mandatory §5.7) and `Temporal` must never un-orphan a
//!   concept, or Stage-3 blast radius would collapse to ~0 on every legal graph;
//!   (2) `c.id <> $node` self-exclusion, matching MemoryStore's skip;
//!   (3) **aged edges only** — `e.created_at <= cutoff` in BOTH queries, and span
//!   also gates `i.created_at <= cutoff` (spec §4.1 second errata); (4) **F1
//!   single-point rule** — `coverage` is `0.0` only when `distinct == 0`; a non-empty
//!   span over a single-point session extent reports `1.0` (canonization Stage 2
//!   parity in short sessions).
//! - **`chunk_group_id` (T2.5) is a first-class column** (P3 review round 1
//!   remediation). `concepts.chunk_group_id STRING` (nullable) is declared in the
//!   `CREATE TABLE` for fresh installs AND added via `ALTER TABLE concepts ADD COLUMN
//!   IF NOT EXISTS chunk_group_id STRING` for existing clusters (Cockroach supports
//!   `IF NOT EXISTS` on `ADD COLUMN`), so `init_schema` stays idempotent either way.
//!   The concept upsert writes it and `load_session` reads it back — a flush→load
//!   cycle now PRESERVES the T5.2 sibling co-retrieval key (regression-locked in the
//!   live conformance suite).
//! - **`GraphSnapshot::embedding` (the `EmbeddingContract`) is durable.** The
//!   sessions table carries `embedding_kind
//!   STRING`, `embedding_model STRING`, `embedding_dim INT` (nullable; same
//!   CREATE + ALTER `ADD COLUMN IF NOT EXISTS` idempotency pattern as
//!   `chunk_group_id`), `seed` upserts all three from the snapshot's contract,
//!   and `load_session` materializes `GraphSnapshot.embedding` when
//!   `embedding_kind` is present (STORE-1 remediation). **Corruption parity
//!   (STORE-7):** a row with exactly one of `embedding_kind` / `embedding_dim`
//!   set (kind XOR dim) is a `Backend` corruption error from `load_session` —
//!   mirroring sqlite — never a silent `None`. `flush` applies the ordered
//!   `Mutation::SetEmbedding` in the same transaction as concept vectors, so
//!   ordinary write-behind and full-snapshot seed converge on the same columns.
//!   The DDL column width *is* read from the schema: `vector_dimensions()` parses
//!   `VECTOR(n)` out of the embedded `001_init.sql` (not a global constant), so
//!   `resolve::check_vector_compatibility` can reject mismatched embedders.
//! - **rustls DSN rewrite.** sqlx's rustls stack cannot open libpq's magic
//!   `sslrootcert=system` path; the `.env` DSN uses it. `dsn_for_rustls` rewrites it
//!   to a real CA bundle (or downgrades `verify-full` → `require`) before pooling
//!   (T0.3 spike, proven against the cloud cluster).

use std::borrow::Cow;

use sqlx::postgres::PgConnectOptions;

use super::{Dialect, PgStore};
use crate::store::{StoreConfig, StoreError};

// The test modules below drive the shared base directly (SQL shapes, pure
// helpers, the live conformance suite), so they read the family's items
// through this glob rather than re-listing three hundred names.
#[cfg(test)]
use super::*;

/// T3.1 DDL, embedded and executed verbatim by [`crate::store::GraphStore::init_schema`].
/// Idempotent by construction (`CREATE ... IF NOT EXISTS` everywhere).
const INIT_SQL: &str = include_str!("../../../migrations/cockroach/001_init.sql");

/// CockroachDB, as a [`Dialect`] of the Postgres-wire-protocol family.
///
/// A zero-sized compile-time selector: it is never constructed, only named as
/// `PgStore<CockroachDialect>`.
pub struct CockroachDialect;

impl Dialect for CockroachDialect {
    /// The schema file **is** the contract here: `001_init.sql` is embedded
    /// verbatim and its `VECTOR(n)` is the width authority, so `dim` is
    /// asserted against the parse rather than substituted into it. A caller
    /// that resolved some other width is a bug in the resolution, not a
    /// migration to perform, so it is refused here rather than provisioned.
    fn init_sql(dim: usize) -> Result<Cow<'static, str>, StoreError> {
        let ddl_dim = ddl_vector_dim()?;
        if dim != ddl_dim {
            return Err(StoreError::Invariant(format!(
                "cockroach schema is VECTOR({ddl_dim}) but the store resolved width {dim}"
            )));
        }
        Ok(Cow::Borrowed(INIT_SQL))
    }

    /// Cockroach's text type is `STRING`; `::TEXT` is an alias it accepts, but
    /// the shipped DDL and every reviewed statement say `STRING`, so B0
    /// preserves that spelling byte for byte.
    const STRING_CAST: &'static str = "::STRING";

    /// Cockroach's dense-vector type is `VECTOR`.
    const VECTOR_CAST: &'static str = "::VECTOR";

    /// `<->` is **L2 distance** on Cockroach (there is no cosine operator), which
    /// is why [`Dialect::distance_to_score`] below squares.
    const DISTANCE_OP: &'static str = "<->";

    /// `spec §4.1` L2 distance → cosine similarity (`1 - d²/2`), clamped to
    /// `[-1, 1]`.
    ///
    /// The identity holds **only for unit-normalized embeddings**: for
    /// `|a| = |b| = 1`, `d² = |a - b|² = 2 - 2·(a·b)`, so `1 - d²/2 = a·b`,
    /// which is cosine. That premise is the `Embedder::embed` output contract
    /// (documented under F), not an accident of the current model. The clamp
    /// absorbs float error at the ends rather than widening the range.
    fn distance_to_score(dist: f64) -> f64 {
        (1.0 - 0.5 * dist * dist).clamp(-1.0, 1.0)
    }

    /// The width authority is the **DDL**, never config: `concepts.embedding`
    /// is `VECTOR(n)` in the shipped `001_init.sql`, so `cfg.vector_dim` (the
    /// operator's pre-ingest pin, B4) is deliberately ignored here. A pin that
    /// disagrees is caught at the serving verbs' resolution boundary, which is
    /// kind-agnostic, so ignoring it here loses no protection.
    fn vector_dim(_cfg: &StoreConfig) -> Result<usize, StoreError> {
        ddl_vector_dim()
    }

    const NAME: &'static str = "cockroach";
    const STORE_TYPE_NAME: &'static str = "CockroachStore";
    const DSN_ENV: &'static str = "LAMBO_COCKROACH_DSN";
    const DSN_LABEL: &'static str = "Cockroach DSN";

    fn post_init_statements() -> &'static [&'static str] {
        // Byte-identical to the two ALTERs B0 ran from PgStore::init_schema.
        &[
            "ALTER TABLE session_leases \
             ADD COLUMN IF NOT EXISTS current_token INT NOT NULL DEFAULT 0",
            "ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING",
        ]
    }

    fn apply_connect_options(options: PgConnectOptions) -> Result<PgConnectOptions, StoreError> {
        let beam = super::vector_beam_size_from_env()?.unwrap_or(super::DEFAULT_VECTOR_BEAM_SIZE);
        Ok(options.options([("vector_search_beam_size", beam.to_string())]))
    }
}

/// The one place the Cockroach width authority is read, shared by
/// [`Dialect::init_sql`]'s assertion and [`Dialect::vector_dim`]'s report so
/// the two can never disagree about what the schema says.
fn ddl_vector_dim() -> Result<usize, StoreError> {
    schema_vector_dim(INIT_SQL).ok_or_else(|| {
        StoreError::Backend(
            "could not parse VECTOR(n) column width from migrations/cockroach/001_init.sql".into(),
        )
    })
}

/// Parse the dense-vector column width out of the DDL (`VECTOR(n)`). The schema is the
/// authority on the width (spec §3.3 "Vector width: not a global constant"), so this is
/// read from the embedded `001_init.sql`, never a separate constant.
fn schema_vector_dim(ddl: &str) -> Option<usize> {
    let start = ddl.find("VECTOR(")?;
    let rest = &ddl[start + "VECTOR(".len()..];
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

/// The Cockroach instantiation of [`DialectSql`], for the SQL-shape tests: the
/// statements below are exactly what `PgStore<CockroachDialect>` issues, so the
/// tests still read the real text rather than a re-spelled copy of it.
#[cfg(test)]
fn crdb_sql() -> DialectSql {
    DialectSql::for_dialect::<CockroachDialect>()
}

/// The durable CockroachDB adapter, under the name every caller already uses.
///
/// The type is `PgStore<CockroachDialect>`: `CockroachStore::new(cfg)` and
/// every `GraphStore` method keep their existing signatures, so B0's extraction
/// is invisible outside `src/store/`.
pub type CockroachStore = PgStore<CockroachDialect>;

// ---------------------------------------------------------------------------
// Pure-logic unit tests (no cluster)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sql_test_ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    /// Minimal rows for the SQL-shape tests. Only the *shape* of the generated
    /// statement is under test, so the values are arbitrary.
    fn test_interaction(id: NodeId) -> Interaction {
        Interaction {
            id,
            session_id: SessionId::from("sql-shape"),
            agent_id: crate::types::AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id: None,
            created_at: sql_test_ts(),
            event_time: None,
        }
    }

    fn test_concept(origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId::new(),
            session_id: SessionId::from("sql-shape"),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: crate::types::AgentId::from("agent-a"),
            created_at: sql_test_ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            chunk_group_id: None,
        }
    }

    fn test_edge(source: NodeId, target: NodeId, edge_type: EdgeType) -> Edge {
        Edge {
            id: NodeId::new(),
            session_id: SessionId::from("sql-shape"),
            source,
            target,
            edge_type,
            weight: 0.5,
            reinforcements: 1,
            created_at: sql_test_ts(),
            last_reinforced: sql_test_ts(),
            event_time: None,
        }
    }

    /// E2E-F2: the Cockroach dialect's DSN variable and the one configuration
    /// resolution reads for `kind = "cockroach"` must be one string.
    #[test]
    fn dsn_env_named_in_errors_is_the_one_config_reads() {
        assert_eq!(CockroachDialect::DSN_ENV, crate::store::COCKROACH_DSN_ENV,);
        assert_eq!(
            crate::store::StoreKind::Cockroach.dsn_env(),
            Some(CockroachDialect::DSN_ENV),
        );
    }

    /// T7.4: the ANN accuracy dial parses and fails closed. A tuning knob that
    /// is silently ignored on a typo is worse than no knob — the operator
    /// believes accuracy was raised when it was not.
    #[test]
    fn vector_beam_size_env_parses_and_fails_closed() {
        let _g = crate::test_util::env_lock();
        let restore = std::env::var(VECTOR_BEAM_SIZE_ENV).ok();

        std::env::remove_var(VECTOR_BEAM_SIZE_ENV);
        assert_eq!(
            vector_beam_size_from_env().unwrap(),
            None,
            "the parser reports absence; the DEFAULT is applied at the call site"
        );
        // Pin the measured default (adve-review MAJOR-1). 32 is CockroachDB's
        // default and measured ~6-7% worse on recall; 256 measured WORSE than
        // 64. If this constant changes, the measurement in its doc must be
        // redone — it is evidence-backed, not a taste call.
        assert_eq!(DEFAULT_VECTOR_BEAM_SIZE, 64);
        assert!(
            (VECTOR_BEAM_SIZE_MIN..=VECTOR_BEAM_SIZE_MAX).contains(&DEFAULT_VECTOR_BEAM_SIZE),
            "default must satisfy the server's own bounds"
        );

        // Exported-but-blank behaves as absent (same convention as LAMBO_STORE).
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "");
        assert_eq!(vector_beam_size_from_env().unwrap(), None);
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "   ");
        assert_eq!(vector_beam_size_from_env().unwrap(), None);

        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "128");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(128));
        // Server-enforced bounds, verified live 2026-08-13.
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "1");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(1));
        std::env::set_var(VECTOR_BEAM_SIZE_ENV, "2048");
        assert_eq!(vector_beam_size_from_env().unwrap(), Some(2048));

        for bad in ["0", "2049", "-1", "64.5", "many", "1e3"] {
            std::env::set_var(VECTOR_BEAM_SIZE_ENV, bad);
            assert!(
                vector_beam_size_from_env().is_err(),
                "{bad:?} must be rejected at pool construction, not silently dropped"
            );
        }

        match restore {
            Some(v) => std::env::set_var(VECTOR_BEAM_SIZE_ENV, v),
            None => std::env::remove_var(VECTOR_BEAM_SIZE_ENV),
        }
    }

    // Vector codec tests (roundtrip / rendering / non-finite) live in the shared
    // `crate::store::vector` module — SQLite stores the same text form, so the codec's
    // coverage must run under either store feature (CON-8).

    #[test]
    fn schema_vector_dim_reads_ddl_width() {
        assert_eq!(schema_vector_dim(INIT_SQL), Some(1024));
        assert_eq!(schema_vector_dim("embedding VECTOR(768)"), Some(768));
        assert_eq!(schema_vector_dim("no vector here"), None);
        assert_eq!(schema_vector_dim("VECTOR(x)"), None);
        // The DDL is the authority: a schema change flows into vector_dimensions().
        assert_eq!(schema_vector_dim(INIT_SQL).unwrap(), 1024);
    }

    /// D/C upgrade path: the served migration must carry idempotent ALTERs for
    /// every column a later wave shipped inline-only in the CREATE TABLEs.
    /// `init_schema` executes INIT_SQL verbatim on every provision, so these
    /// ALTER statements ARE the convergence path for a cluster provisioned by
    /// an older build; without them the column preflight refuses the store with
    /// no self-repair ("table edges is missing a column ... event_time").
    /// Text-level contract — needs no live cluster (live convergence is the
    /// ignored suite's job).
    #[test]
    fn served_migration_converges_event_time_and_human_confirmed() {
        assert!(
            INIT_SQL.contains(
                "ALTER TABLE interactions ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ",
            ),
            "pre-D interactions rows have no convergence path"
        );
        assert!(
            INIT_SQL.contains("ALTER TABLE edges ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ"),
            "pre-D edges rows have no convergence path"
        );
        assert!(
            INIT_SQL.contains(
                "ALTER TABLE concepts ADD COLUMN IF NOT EXISTS human_confirmed INT NOT NULL DEFAULT 0",
            ),
            "pre-C concepts rows have no convergence path"
        );
        assert_eq!(
            CockroachDialect::post_init_statements(),
            [
                "ALTER TABLE session_leases \
                 ADD COLUMN IF NOT EXISTS current_token INT NOT NULL DEFAULT 0",
                "ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING",
            ]
        );
    }

    #[test]
    fn dsn_for_rustls_rewrites_sslrootcert_system() {
        let out =
            dsn_for_rustls("postgresql://u:p@h:26257/db?sslmode=verify-full&sslrootcert=system");
        assert!(!out.contains("sslrootcert=system"), "{out}");
        let has_bundle = [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
            "/etc/ssl/ca-bundle.pem",
        ]
        .iter()
        .any(|p| std::path::Path::new(p).is_file());
        if has_bundle {
            assert!(
                out.contains("sslrootcert=/") && !out.contains("system"),
                "{out}"
            );
        } else {
            assert!(!out.contains("sslmode=verify-full"), "downgraded: {out}");
        }
        // Dangling separators cleaned.
        assert!(
            !out.contains("?&") && !out.ends_with('&') && !out.ends_with('?'),
            "{out}"
        );
        // Untouched DSN passes through unchanged.
        let plain = "postgresql://u:p@h:26257/db?sslmode=require";
        assert_eq!(dsn_for_rustls(plain), plain);
    }

    #[test]
    fn age_cutoff_computation() {
        let now = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
        assert_eq!(cutoff(now, Duration::ZERO).unwrap(), now);
        assert_eq!(
            cutoff(now, Duration::from_secs(3600)).unwrap(),
            now - chrono::Duration::hours(1)
        );
        assert_eq!(
            cutoff(now, Duration::from_secs(90)).unwrap(),
            now - chrono::Duration::seconds(90)
        );
        // Out-of-range (chrono i64 seconds) -> typed error, not panic.
        assert!(cutoff(now, Duration::from_secs(u64::MAX)).is_err());
    }

    #[test]
    fn distance_to_score_is_cosine() {
        use CockroachDialect as C;
        assert_eq!(C::distance_to_score(0.0), 1.0);
        assert!((C::distance_to_score(1.0) - 0.5).abs() < 1e-12);
        assert_eq!(C::distance_to_score(2.0), -1.0);
        assert_eq!(C::distance_to_score(3.0), -1.0, "clamped");
        assert!((C::distance_to_score(0.5) - 0.875).abs() < 1e-12);
        // B3 vice-versa pin: copying Postgres `1 - d` onto Cockroach goes red.
        // Postgres at d=1 is 0.0 and at d=0.5 is 0.5.
        assert_ne!(
            C::distance_to_score(1.0),
            0.0,
            "copied Postgres 1 - d onto Cockroach L2"
        );
        assert_ne!(
            C::distance_to_score(0.5),
            0.5,
            "copied Postgres 1 - d onto Cockroach L2"
        );
        assert!(
            C::forced_exact_scan_sql().is_none(),
            "Cockroach has no H3 forced-exact GUC"
        );
    }

    #[test]
    fn embedding_dim_check() {
        assert!(check_embedding_dim(&[0.0; 1024], 1024).is_ok());
        let err = check_embedding_dim(&[0.0; 8], 1024).unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
    }
    #[test]
    fn session_filter_keeps_only_caller_and_preserves_order() {
        // Rows arrive from SQL in L2-distance-ascending order (score-descending).
        // Foreign-session rows must be dropped without reordering the survivors.
        let sid = SessionId::from("caller-session");
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let (fx, fy) = (NodeId::new(), NodeId::new());
        let foreign = "other-session".to_string();
        let mine = sid.0.clone();
        // dist asc: a(0.0), fx(0.5), b(1.0), fy(1.5), c(2.0)
        let rows = vec![
            (a, 0.0, mine.clone()),
            (fx, 0.5, foreign.clone()),
            (b, 1.0, mine.clone()),
            (fy, 1.5, foreign.clone()),
            (c, 2.0, mine.clone()),
        ];
        let got = filter_session_rows::<CockroachDialect>(&sid, &rows);
        let items: Vec<_> = got.iter().map(|s| s.item).collect();
        assert_eq!(
            items,
            vec![a, b, c],
            "foreign rows dropped, order preserved"
        );
        // Scores follow distance_to_score: 0.0→1.0, 1.0→0.5, 2.0→-1.0 (descending).
        assert!((got[0].score - 1.0).abs() < 1e-12);
        assert!((got[1].score - 0.5).abs() < 1e-12);
        assert!((got[2].score - (-1.0)).abs() < 1e-12);
    }

    #[test]
    fn vector_ties_are_ordered_by_uuid_or_trigger_exact_fallback() {
        let sid = SessionId::from("ties");
        let low = NodeId(Uuid::from_u64_pair(0, 1));
        let mid = NodeId(Uuid::from_u64_pair(0, 2));
        let high = NodeId(Uuid::from_u64_pair(0, 3));
        let rows_a = vec![
            (high, 0.25, sid.0.clone()),
            (low, 0.25, sid.0.clone()),
            (mid, 0.25, sid.0.clone()),
        ];
        let mut rows_b = rows_a.clone();
        rows_b.reverse();
        let ids = |rows: &[(NodeId, f64, String)]| {
            filter_session_rows::<CockroachDialect>(&sid, rows)
                .into_iter()
                .map(|s| s.item)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&rows_a), vec![low, mid, high]);
        assert_eq!(ids(&rows_b), vec![low, mid, high]);

        // More equal-distance rows than the fetch window: k+1 exposes that the
        // kth subset is arbitrary, so the caller must use exact fallback.
        assert!(has_boundary_tie(&rows_a, 2));
        assert!(has_boundary_tie(&rows_b, 2));
        assert!(crdb_sql()
            .session_vector_candidates
            .contains("ORDER BY dist ASC, id ASC"));
        assert!(!has_boundary_tie(
            &[
                (low, 0.1, sid.0.clone()),
                (mid, 0.2, sid.0.clone()),
                (high, 0.3, sid.0.clone()),
            ],
            2
        ));
    }

    #[test]
    fn grow_retry_is_final_when_satisfied_exhausted_or_capped() {
        // Satisfied: enough in-session hits -> final, no retry.
        assert_eq!(next_fetch_k(3, true, 30, 3), None);
        // Exhausted: no k+1 lookahead -> no more global rows exist.
        assert_eq!(next_fetch_k(1, false, 30, 3), None);
        // Capped: k already at the cap -> never grow past it.
        assert_eq!(next_fetch_k(0, true, VECTOR_FETCH_CAP, 5), None);
        // Page full + under-delivered + room to grow -> double (capped at VECTOR_FETCH_CAP).
        assert_eq!(
            next_fetch_k(1, true, 30, 5),
            Some(60),
            "grow retry doubles k"
        );
        let near_cap = VECTOR_FETCH_CAP / 2;
        assert_eq!(
            next_fetch_k(1, true, near_cap, 5),
            Some(VECTOR_FETCH_CAP),
            "growth clamps at the cap"
        );
    }

    #[test]
    fn cap_crowd_out_uses_exact_session_fallback() {
        // Adversarial distribution: 2,048 closer foreign-session rows put the
        // caller's nearest concept at global rank 2,049. The capped fast path
        // must not silently return empty; it switches to the exact session query.
        let caller = SessionId::from("caller");
        let mut globally_ranked: Vec<(NodeId, f64, String)> = (0..VECTOR_FETCH_CAP)
            .map(|rank| (NodeId::new(), rank as f64 / 10_000.0, "foreign".to_string()))
            .collect();
        globally_ranked.push((NodeId::new(), 0.3, caller.0.clone()));
        let capped_page = &globally_ranked[..VECTOR_FETCH_CAP];
        let local = filter_session_rows::<CockroachDialect>(&caller, capped_page);
        assert!(local.is_empty(), "local row is exactly global rank 2,049");
        assert!(needs_session_fallback(
            local.len(),
            true,
            VECTOR_FETCH_CAP,
            1
        ));
        let crdb = crdb_sql();
        assert!(crdb
            .session_vector_candidates
            .contains("WHERE session_id = $2"));
        assert!(crdb
            .session_vector_candidates
            .contains("ORDER BY dist ASC, id ASC"));

        assert!(!needs_session_fallback(1, true, VECTOR_FETCH_CAP, 1));
        assert!(!needs_session_fallback(0, false, VECTOR_FETCH_CAP, 1));
    }

    #[test]
    fn initial_fetch_k_is_floor_but_capped_at_same_bound_as_growth() {
        // The BASE global fetch must be floored at the multiplier (a non-trivial
        // query always pulls some headroom) and CAPPED at VECTOR_FETCH_CAP — the
        // same worst-case bound the growth step enforces. The public caller validates
        // `limit`; this pure helper still saturates defensively. `limit == 0` is
        // short-circuited by the caller, so 0 maps to
        // the floor here.
        assert_eq!(
            initial_fetch_k(0),
            VECTOR_FETCH_MULTIPLIER,
            "0 floors at multiplier"
        );
        assert_eq!(
            initial_fetch_k(1),
            VECTOR_FETCH_MULTIPLIER,
            "floor at multiplier"
        );
        assert_eq!(initial_fetch_k(5), 50);
        assert_eq!(initial_fetch_k(7), 70);
        let over = VECTOR_FETCH_CAP / VECTOR_FETCH_MULTIPLIER + 1;
        assert_eq!(
            initial_fetch_k(over),
            VECTOR_FETCH_CAP,
            "limit just over cap clamps"
        );
        assert_eq!(
            initial_fetch_k(usize::MAX),
            VECTOR_FETCH_CAP,
            "defensive saturation clamps to the cap"
        );
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn oversized_seed_embedding_dimension_fails_before_pool_use() {
        let store = CockroachStore::new(StoreConfig {
            kind: crate::store::StoreKind::Cockroach,
            dsn: Some("postgresql://localhost:26257/defaultdb?sslmode=disable".into()),
            path: None,
            vector_dim: None,
        })
        .unwrap();
        let err = store
            .seed(&GraphSnapshot {
                session_id: SessionId::from("oversized-dim"),
                embedding: Some(EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: usize::MAX,
                }),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
        assert!(
            store.pool.get().is_none(),
            "dimension check precedes pool use"
        );
    }

    /// Count `$n` placeholders in a statement and return the max `n` (1-based).
    fn placeholder_max(sql: &str) -> usize {
        let mut max = 0;
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'$' && bytes[i + 1].is_ascii_digit() {
                let mut n = 0usize;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    n = n * 10 + (bytes[j] - b'0') as usize;
                    j += 1;
                }
                max = max.max(n);
                i = j;
            } else {
                i += 1;
            }
        }
        max
    }

    /// Build the concept upsert for `n` rows and hand back its SQL text.
    fn concept_sql_for(n: usize) -> String {
        let iid = NodeId::new();
        let concepts: Vec<Concept> = (0..n)
            .map(|k| {
                let mut c = test_concept(iid, &format!("c{k}"));
                c.embedding = Some(vec![0.5; 4]);
                c
            })
            .collect();
        let rows: Vec<ConceptRow<'_>> = concepts.iter().map(ConceptRow::new).collect();
        let embeddings: Vec<Option<String>> = vec![Some("[0.5,0.5,0.5,0.5]".into()); n];
        concept_upsert_query(&rows, &embeddings, CockroachDialect::VECTOR_CAST)
            .sql()
            .to_string()
    }

    /// B0-R1-1. Standing pin: every composed statement, the vector-cast token,
    /// and `keyword_candidates_sql` for n = 1..5 are byte-identical to the
    /// pre-carve constants in commit `7937de7`. The `PRE_*` bodies were
    /// generated by a parser over `git show 7937de7:src/store/cockroach.rs`,
    /// not transcribed by hand. A single whitespace change in any composed
    /// statement, or a change to `STRING_CAST`, `VECTOR_CAST`, or
    /// `DISTANCE_OP`, must fail here.
    #[test]
    fn b0_composed_sql_is_byte_identical_to_the_pre_carve_constants() {
        const PRE_VECTOR_CANDIDATES_SQL: &str = r#"
SELECT id::STRING AS id, session_id::STRING AS session_id,
       embedding <-> $1::VECTOR AS dist
FROM concepts
WHERE embedding IS NOT NULL
ORDER BY dist ASC
LIMIT $2
"#;

        const PRE_SESSION_VECTOR_CANDIDATES_SQL: &str = r#"
SELECT id::STRING AS id, embedding <-> $1::VECTOR AS dist
FROM concepts
WHERE session_id = $2 AND embedding IS NOT NULL
ORDER BY dist ASC, id ASC
LIMIT $3
"#;

        const PRE_UPSERT_SESSION_SQL: &str = r#"
INSERT INTO sessions (
    session_id, root_goal, created_at, closed_at,
    embedding_kind, embedding_model, embedding_dim
) VALUES ($1, $2::JSONB, COALESCE($3, now()), $4, $5::STRING, $6::STRING, $7::INT)
ON CONFLICT (session_id) DO UPDATE SET
    root_goal = EXCLUDED.root_goal,
    created_at = EXCLUDED.created_at,
    closed_at = EXCLUDED.closed_at,
    embedding_kind = EXCLUDED.embedding_kind,
    embedding_model = EXCLUDED.embedding_model,
    embedding_dim = EXCLUDED.embedding_dim
"#;

        const PRE_SET_EMBEDDING_SQL: &str = r#"
UPDATE sessions
SET embedding_kind = $2::STRING,
    embedding_model = $3::STRING,
    embedding_dim = $4::INT
WHERE session_id = $1
"#;

        const PRE_SELECT_SESSION_SQL: &str = r#"
SELECT root_goal::STRING AS root_goal, created_at, closed_at,
       embedding_kind, embedding_model, embedding_dim
FROM sessions
WHERE session_id = $1
"#;

        const PRE_SELECT_INTERACTIONS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, agent_id, prompt_text,
       previous_id::STRING AS previous_id, created_at, event_time
FROM interactions
WHERE session_id = $1
ORDER BY created_at, id
"#;

        const PRE_SELECT_CONCEPTS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, content, canonical_key, concept_type,
       origin_interaction::STRING AS origin_interaction, origin_agent, created_at,
       access_count, last_accessed, gc_survived, canonization_status, blast_radius,
       last_demotion_time, embedding::STRING AS embedding, chunk_group_id, human_confirmed
FROM concepts
WHERE session_id = $1
ORDER BY id
"#;

        const PRE_SELECT_EDGES_SQL: &str = r#"
SELECT id::STRING AS id, session_id, source::STRING AS source,
       target::STRING AS target, edge_type, weight, reinforcements,
       created_at, last_reinforced, event_time
FROM edges
WHERE session_id = $1
ORDER BY id
"#;

        const PRE_SELECT_CANONIZATION_EVENTS_SQL: &str = r#"
SELECT id::STRING AS id, session_id, node_id::STRING AS node_id,
       from_status, to_status, blast_radius, last_demotion_time, occurred_at
FROM canonization_events
WHERE session_id = $1
ORDER BY occurred_at, id
"#;

        const PRE_SELECT_RESERVATIONS_SQL: &str = r#"
SELECT session_id, node_id::STRING AS node_id, agent_id, expires_at
FROM reservations
WHERE session_id = $1
"#;

        let sql = crdb_sql();
        assert_eq!(
            sql.vector_candidates, PRE_VECTOR_CANDIDATES_SQL,
            "vector_candidates"
        );
        assert_eq!(
            sql.session_vector_candidates, PRE_SESSION_VECTOR_CANDIDATES_SQL,
            "session_vector_candidates"
        );
        assert_eq!(sql.upsert_session, PRE_UPSERT_SESSION_SQL, "upsert_session");
        assert_eq!(sql.set_embedding, PRE_SET_EMBEDDING_SQL, "set_embedding");
        assert_eq!(sql.select_session, PRE_SELECT_SESSION_SQL, "select_session");
        assert_eq!(
            sql.select_interactions, PRE_SELECT_INTERACTIONS_SQL,
            "select_interactions"
        );
        assert_eq!(
            sql.select_concepts, PRE_SELECT_CONCEPTS_SQL,
            "select_concepts"
        );
        assert_eq!(sql.select_edges, PRE_SELECT_EDGES_SQL, "select_edges");
        assert_eq!(
            sql.select_canonization_events, PRE_SELECT_CANONIZATION_EVENTS_SQL,
            "select_canonization_events"
        );
        assert_eq!(
            sql.select_reservations, PRE_SELECT_RESERVATIONS_SQL,
            "select_reservations"
        );
        assert_eq!(sql.vector_cast, "::VECTOR", "vector_cast");

        // Pre-carve `keyword_candidates_sql` builder, copied from 7937de7
        // (the `::STRING` prefix was a literal then; it is now `STRING_CAST`).
        fn pre_keyword_candidates_sql(n_tokens: usize) -> String {
            debug_assert!(n_tokens > 0);
            let mut sql = String::with_capacity(64 + n_tokens * 96);
            sql.push_str(
                "SELECT id::STRING AS id, content, canonical_key FROM concepts WHERE session_id = $1 AND (",
            );
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
        for n in 1..=5 {
            assert_eq!(
                keyword_candidates_sql::<CockroachDialect>(n),
                pre_keyword_candidates_sql(n),
                "keyword_candidates_sql n={n}"
            );
        }
    }

    #[test]
    fn upsert_placeholder_shapes_match_structs() {
        // Snapshot->row mapping shapes: every struct column has exactly one placeholder
        // and the column counts match the INSERT column lists. Upserts are built
        // per-call by `QueryBuilder` now (L82-1), so a single row is the shape
        // the old fixed-placeholder statements had.
        let iid = NodeId::new();
        let i = test_interaction(iid);
        // Asserted against the shared column constants, not bare literals: those
        // constants are what the per-adapter bind-parameter const-asserts divide
        // the backend limit by (R1-4), so a column added to a statement without
        // updating them must fail here rather than silently widen a chunk past
        // the limit.
        assert_eq!(
            placeholder_max(interaction_upsert_query(&[&i]).sql()),
            INTERACTION_COLUMNS
        );
        assert_eq!(placeholder_max(&concept_sql_for(1)), CONCEPT_COLUMNS);
        let e = test_edge(iid, iid, EdgeType::Derives);
        assert_eq!(
            placeholder_max(edge_upsert_query(&[&e]).sql()),
            EDGE_COLUMNS
        );
        // STORE-1: the full-snapshot upsert now carries the embedding contract
        // (kind/model/dim) alongside root_goal/created_at/closed_at.
        let upsert_session = crdb_sql().upsert_session;
        assert_eq!(placeholder_max(&upsert_session), 7);
        assert!(upsert_session.contains("embedding_kind = EXCLUDED.embedding_kind"));
        assert!(upsert_session.contains("embedding_dim = EXCLUDED.embedding_dim"));
        assert_eq!(placeholder_max(INSERT_CANONIZATION_EVENT_SQL), 8);
        assert_eq!(placeholder_max(UPDATE_CONCEPT_STATUS_SQL), 5);
        assert_eq!(placeholder_max(UPSERT_SYNONYM_SQL), 3);
        assert_eq!(placeholder_max(UPSERT_RESERVATION_SQL), 4);
        // COH-3: the canonization surface carries last_demotion_time end to end.
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("COALESCE($5, last_demotion_time)"));
        assert!(INSERT_CANONIZATION_EVENT_SQL.contains("last_demotion_time"));
        // The vector column carries the ::VECTOR cast; chunk_group_id (T2.5) is
        // the 16th, nullable; human_confirmed (C2) closes the list as the 17th
        // — all included in the conflict UPDATE.
        let concept_sql = concept_sql_for(1);
        assert!(concept_sql.contains("$15::VECTOR"), "{concept_sql}");
        assert!(concept_sql.contains("embedding = EXCLUDED.embedding"));
        assert!(concept_sql.contains("chunk_group_id = EXCLUDED.chunk_group_id"));
        assert!(
            concept_sql.contains("human_confirmed"),
            "the C2 count rides the upsert: {concept_sql}"
        );
        // Edge conflict targets the natural key; id is replaceable on conflict.
        assert!(edge_upsert_query(&[&e])
            .sql()
            .contains("ON CONFLICT (source, target, edge_type)"));
    }

    /// **L82-1.** The flush issues one *multi-row* statement per planned chunk,
    /// and the generated SQL is the half of that change no test on this machine
    /// can put in front of a cluster — so it is asserted directly.
    ///
    /// Three rows must produce 48 placeholders in three `VALUES` tuples, carry
    /// the `::VECTOR` cast on *each* row's embedding placeholder (the cast is
    /// part of the value expression, not the statement), and end in exactly one
    /// `ON CONFLICT` clause.
    #[test]
    fn sql_shape_is_a_multi_row_upsert() {
        let sql = concept_sql_for(3);
        assert_eq!(
            placeholder_max(&sql),
            51,
            "3 rows x 17 columns, numbered across the whole statement: {sql}"
        );
        for n in [15, 32, 49] {
            assert!(
                sql.contains(&format!("${n}::VECTOR")),
                "every row's embedding placeholder needs its own cast, missing ${n}: {sql}"
            );
        }
        assert_eq!(
            sql.matches("ON CONFLICT").count(),
            1,
            "the conflict clause is appended once, after the whole VALUES list: {sql}"
        );
        assert_eq!(
            sql.matches("INSERT INTO concepts").count(),
            1,
            "one statement, not three: {sql}"
        );

        // Edges keep the natural-key conflict target across rows.
        let a = NodeId::new();
        let edges = [
            test_edge(a, NodeId::new(), EdgeType::Causal),
            test_edge(a, NodeId::new(), EdgeType::Dependency),
        ];
        let refs: Vec<&Edge> = edges.iter().collect();
        let edge_sql = edge_upsert_query(&refs).sql().to_string();
        assert_eq!(placeholder_max(&edge_sql), 20, "2 rows x 10 columns");
        assert_eq!(edge_sql.matches("ON CONFLICT").count(), 1);
    }

    /// D-R1-2 (no live cluster: SQL text is the contract). A **non-NULL**
    /// event_time must survive the whole statement path: bound as the LAST
    /// column of both multi-row upserts (matching the column list, so a bind
    /// that drifted onto `created_at`'s slot changes the placeholder count),
    /// carried by `DO UPDATE SET` so the natural-key replace cannot erase it,
    /// and read back by name in both SELECTs. The sqlite adapter reads this
    /// column positionally — this is where a column-order regression shows.
    #[test]
    fn event_time_rides_the_upsert_and_select_shape() {
        let stamped_at = Utc.timestamp_opt(946_684_799, 0).unwrap();

        let mut i = test_interaction(NodeId::new());
        i.event_time = Some(stamped_at);
        let i_sql = interaction_upsert_query(&[&i]).sql().to_string();
        assert_eq!(placeholder_max(&i_sql), 7, "1 row x 7 columns: {i_sql}");
        assert!(
            i_sql.contains("previous_id, created_at, event_time"),
            "event_time closes the INSERT column list: {i_sql}"
        );

        let mut e = test_edge(NodeId::new(), NodeId::new(), EdgeType::Causal);
        e.event_time = Some(stamped_at);
        let e_sql = edge_upsert_query(&[&e]).sql().to_string();
        assert_eq!(placeholder_max(&e_sql), 10, "1 row x 10 columns: {e_sql}");
        assert!(
            e_sql.contains("last_reinforced, event_time"),
            "event_time closes the INSERT column list: {e_sql}"
        );

        // Whole-record replace on conflict is only consistent while BOTH
        // halves carry the stamp (I2 convention).
        for sql in [&i_sql, &e_sql] {
            let (_, on_conflict) = sql
                .split_once("ON CONFLICT")
                .expect("the upsert has a conflict clause");
            assert!(
                on_conflict.contains("event_time = EXCLUDED.event_time"),
                "conflict update must re-stamp from the incoming row: {sql}"
            );
        }

        let crdb = crdb_sql();
        for sql in [&crdb.select_interactions, &crdb.select_edges] {
            assert!(
                sql.contains("event_time"),
                "load must read event_time back by name: {sql}"
            );
        }
    }

    /// C2 (no live cluster: SQL text is the contract). `human_confirmed` —
    /// the solo score's persisted input — must ride the whole statement path:
    /// bound as the LAST concept column (a bind drifting onto
    /// `chunk_group_id`'s slot changes the placeholder count), carried by
    /// `DO UPDATE SET` so a whole-record replace cannot reset a confirmed
    /// concept to never-confirmed, and read back by name in the SELECT. The
    /// sqlite adapter reads this column positionally (`try_get(16)`) — that is
    /// where an order regression shows.
    #[test]
    fn human_confirmed_rides_the_concept_upsert_and_select_shape() {
        let mut c = test_concept(NodeId::new(), "load-bearing warning");
        c.human_confirmed = 7;
        let sql = concept_upsert_query(
            &[crate::store::batch::ConceptRow::new(&c)],
            &[None],
            CockroachDialect::VECTOR_CAST,
        )
        .sql()
        .to_string();
        assert_eq!(placeholder_max(&sql), 17, "1 row x 17 columns: {sql}");
        assert!(
            sql.contains("chunk_group_id, human_confirmed"),
            "human_confirmed closes the INSERT column list: {sql}"
        );
        let (_, on_conflict) = sql
            .split_once("ON CONFLICT")
            .expect("the upsert has a conflict clause");
        assert!(
            on_conflict.contains("human_confirmed = EXCLUDED.human_confirmed"),
            "conflict update must carry the count from the incoming row: {sql}"
        );
        assert!(
            crdb_sql().select_concepts.contains("human_confirmed"),
            "load must read human_confirmed back by name"
        );
    }

    /// R2-1 (no live cluster: SQL text is the contract). The three
    /// canonization columns are INSERT-only — present in the column list so a
    /// brand-new row carries them, absent from `DO UPDATE SET` so a stale
    /// `Mutation::UpsertNode` snapshot cannot take an already-recorded hop
    /// back out of the row (or erase a demotion cooldown). `gc_survived`,
    /// which shares the same appenders, must still be updated: the property
    /// is column ownership, not a blanket skip.
    ///
    /// The multi-row rewrite (L82-1) is exactly where this could have been lost,
    /// so the assertion runs against the generated statement rather than a
    /// constant — and `store::batch` carries the matching property for the
    /// *values*: a deduplicated row takes its canonization columns from the
    /// first occurrence, which is what row-by-row replay left in the row.
    #[test]
    fn concept_upsert_does_not_write_the_canonization_columns_on_conflict() {
        let sql = concept_sql_for(2);
        let (insert, on_conflict) = sql
            .split_once("ON CONFLICT")
            .expect("the concept upsert has a conflict clause");
        for col in ["canonization_status", "blast_radius", "last_demotion_time"] {
            assert!(
                insert.contains(col),
                "{col} must stay in the INSERT column list (new rows carry it)"
            );
            assert!(
                !on_conflict.contains(col),
                "{col} is written only by the canonization path (R2-1)"
            );
        }
        assert!(
            sql.contains("gc_survived = EXCLUDED.gc_survived"),
            "the upsert's own columns must still update on conflict"
        );
        // The canonization path is that single writer.
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("canonization_status"));
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("blast_radius"));
        assert!(UPDATE_CONCEPT_STATUS_SQL.contains("last_demotion_time"));
    }

    #[test]
    fn structural_query_placeholder_order_and_counts() {
        // §4.1 SQL construction: exactly three binds, in (session, node, cutoff) order.
        assert_eq!(placeholder_max(BLAST_RADIUS_SQL), 3);
        assert_eq!(placeholder_max(INTERACTION_SPAN_SQL), 3);
        for sql in [BLAST_RADIUS_SQL, INTERACTION_SPAN_SQL] {
            let p1 = sql.find("$1").unwrap();
            let p2 = sql.find("$2").unwrap();
            let p3 = sql.find("$3").unwrap();
            assert!(p1 < p2 && p2 < p3, "placeholders must appear in bind order");
        }
        // Errata: structural edge types only; provenance must not appear.
        for sql in [BLAST_RADIUS_SQL, INTERACTION_SPAN_SQL] {
            assert!(
                sql.contains("'Dependency', 'Causal', 'Hierarchical'"),
                "{sql}"
            );
            assert!(!sql.contains("Derives"), "{sql}");
            assert!(!sql.contains("Temporal"), "{sql}");
            // Concept-sourced only: source JOIN pins src to a concept row.
            assert!(sql.contains("JOIN concepts src"), "{sql}");
        }
        // MemoryStore parity: interaction_span filters BOTH edge and interaction
        // age — each resolved through D's fallback rule (COALESCE with event_time).
        assert!(INTERACTION_SPAN_SQL.contains("COALESCE(e.event_time, e.created_at) <= $3"));
        assert!(INTERACTION_SPAN_SQL.contains("COALESCE(i.event_time, i.created_at) <= $3"));
    }

    /// F5: `origin_interaction` is a global FK, so the span CTE must scope the
    /// joined interaction to the queried session — the extent CTE already is.
    /// Without it, concepts pointing at another session's interactions inflate
    /// `distinct` and push the ratio above 1.0 (MemoryStore never sees them).
    /// Text-level like its `structural_query_placeholder_order_and_counts`
    /// neighbour: the behavioural twin runs on SQLite
    /// (`interaction_span_ignores_cross_session_origin_interactions`), which
    /// needs no live cluster.
    #[test]
    fn span_sql_is_session_scoped_and_coverage_is_clamped() {
        assert!(
            INTERACTION_SPAN_SQL.contains("i.session_id = $1"),
            "span CTE must scope the origin interaction to the session (F5)"
        );
        assert!(
            INTERACTION_SPAN_SQL.contains("least(1.0, greatest(0.0,"),
            "coverage must be clamped to [0,1] like MemoryStore/SQLite (F5)"
        );
    }

    #[test]
    fn keyword_sql_placeholder_counts_and_order() {
        for n in [1usize, 2, 3, 7] {
            let sql = keyword_candidates_sql::<CockroachDialect>(n);
            // $1 (session) once; each of $2..$n+1 (token) exactly twice (content + key).
            assert_eq!(placeholder_max(&sql), n + 1);
            let mut counts = std::collections::HashMap::new();
            for m in sql.split('$').skip(1) {
                let digits: String = m.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() {
                    *counts
                        .entry(digits.parse::<usize>().unwrap())
                        .or_insert(0usize) += 1;
                }
            }
            assert_eq!(counts[&1], 1);
            for k in 2..=n + 1 {
                assert_eq!(
                    counts[&k], 2,
                    "token placeholder ${k} used for content and key"
                );
            }
        }
        // No LIKE wildcards: strpos(lower(...)) is exact substring (MemoryStore contains).
        let sql = keyword_candidates_sql::<CockroachDialect>(1);
        assert!(sql.contains("strpos(lower(content), $2) > 0"));
        assert!(!sql.contains("ILIKE") && !sql.contains("LIKE"), "{sql}");
    }

    #[test]
    fn enum_column_strings_roundtrip_all_variants() {
        for ct in [
            ConceptType::Entity,
            ConceptType::Logic,
            ConceptType::Constraint,
            ConceptType::Resource,
            ConceptType::Observation,
        ] {
            assert_eq!(parse_concept_type(concept_type_sql(ct)).unwrap(), ct);
        }
        for et in [
            EdgeType::Temporal,
            EdgeType::Derives,
            EdgeType::CoOccurrence,
            EdgeType::Causal,
            EdgeType::Dependency,
            EdgeType::Hierarchical,
            EdgeType::Semantic,
        ] {
            assert_eq!(parse_edge_type(edge_type_sql(et)).unwrap(), et);
        }
        for cs in [
            CanonizationStatus::None,
            CanonizationStatus::Candidate,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
        ] {
            assert_eq!(
                parse_canonization_status(canonization_status_sql(cs)).unwrap(),
                cs
            );
        }
        assert!(parse_concept_type("Bogus").is_err());
        assert!(parse_edge_type("Bogus").is_err());
        assert!(parse_canonization_status("Bogus").is_err());
    }

    #[test]
    fn tx_retryable_is_structured_not_substring() {
        // STORE-4: the tx-replay decision matches the TYPED error — never
        // message text. Constraint violations (SQLSTATE 23xxx) are
        // deterministic: never replayed, dead-lettered upstream. Typed
        // variants are permanent. A Backend error may be a transient
        // (serialization conflict, connection exception, server shutdown);
        // the replay is bounded by TX_RETRY_ATTEMPTS + backoff, so a
        // non-constraint Backend (e.g. a schema bug) at worst re-runs the tx
        // body a bounded number of times before surfacing.
        assert!(!tx_retryable(&StoreError::Constraint("23505".into())));
        assert!(!tx_retryable(&StoreError::SessionNotFound("x".into())));
        assert!(!tx_retryable(&StoreError::Capability("nope".into())));
        assert!(!tx_retryable(&StoreError::NotFound("nope".into())));
        assert!(!tx_retryable(&StoreError::Invariant("nope".into())));
        assert!(tx_retryable(&StoreError::Backend(
            "restart transaction: TransactionRetryWithProtoRefreshError: TransactionRetryError: \
             retry txn (RETRY_SERIALIZABLE - failed preemptive refresh...)"
                .into(),
        )));
        assert!(tx_retryable(&StoreError::Backend(
            "db error: SQLSTATE 40001".into()
        )));
        assert!(tx_retryable(&StoreError::Backend(
            "relation \"concepts\" does not exist".into()
        )));
    }

    #[tokio::test]
    async fn checked_vector_transaction_retries_backend_but_not_contract_mismatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Models the complete checked candidate transaction closure: the first
        // serializable attempt is aborted with SQLSTATE 40001 and the whole body
        // is invoked again, not resumed after the failed statement.
        let attempts = AtomicUsize::new(0);
        let value = tx_retry(|| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    Err(StoreError::Backend("db error: SQLSTATE 40001".into()))
                } else {
                    Ok(42usize)
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        // The checked read maps a durable/query contract mismatch to Invariant,
        // so it is deterministic and returned on the first attempt.
        let mismatch_attempts = AtomicUsize::new(0);
        let err = tx_retry(|| {
            mismatch_attempts.fetch_add(1, Ordering::SeqCst);
            async {
                Err::<(), _>(StoreError::Invariant(
                    "vector candidate lookup refused after embedding contract changed".into(),
                ))
            }
        })
        .await
        .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
        assert_eq!(mismatch_attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn keyword_score_folds_case_like_memory_store() {
        // Regression (P3 review R1): the SQL predicate lowercases the columns, so the
        // score must fold the row text the same way — a mixed-case row ("Register
        // User") selected by token "register" scores its hits, not 0.0.
        assert_eq!(
            score_keyword_hits("Register User", "Register User", &["register".into()]),
            1,
            "mixed-case content + key must still count the lowercase token"
        );
        assert_eq!(
            score_keyword_hits(
                "Register User",
                "Register User",
                &["register".into(), "user".into()]
            ),
            2
        );
        assert_eq!(
            score_keyword_hits("Register User", "Register User", &["schema".into()]),
            0
        );
        assert_eq!(
            score_keyword_hits("register user", "register user", &["register".into()]),
            1
        );
        // Key-only hit still counts (SQL predicate is content OR canonical_key).
        assert_eq!(
            score_keyword_hits("Foo", "register user", &["register".into()]),
            1
        );
        // Tokens are pre-normalized lowercase; an uppercase token matches nothing.
        assert_eq!(
            score_keyword_hits("register user", "register user", &["Register".into()]),
            0
        );
        // Empty rows/tokens (post-normalization) contribute nothing.
        assert_eq!(score_keyword_hits("", "", &["a".into()]), 0);
    }

    #[test]
    fn normalize_tokens_matches_memory_store() {
        let tokens = vec!["  Schema ".to_string(), "".to_string(), "  ".to_string()];
        assert_eq!(
            CockroachStore::normalize_tokens(&tokens),
            vec!["schema".to_string()]
        );
        assert!(CockroachStore::normalize_tokens(&[]).is_empty());
    }

    #[test]
    fn session_embedding_xor_corruption_errors_not_silent_none() {
        // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
        // (what direct SQL on a corrupt/migrated row would produce) must error like
        // sqlite, not silently return `embedding: None`.
        let sid = "session-store7";
        // The old silent-None shape (kind absent, dim present) — the STORE-7 bug.
        let err = session_embedding_from_parts(None, None, Some(1024), sid).unwrap_err();
        // E2E-2: deterministic corruption classifies as `Invariant`, so
        // `tx_retry` returns on the first attempt instead of replaying it 5×.
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("embedding_dim without embedding_kind"),
            "corruption error must name the shape: {err}"
        );
        // Mirror image: kind present, dim absent — sqlite errors here too.
        let err = session_embedding_from_parts(Some("bge_m3".into()), None, None, sid).unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
        assert!(
            err.to_string()
                .contains("embedding_kind without embedding_dim"),
            "{err}"
        );
        // Negative dim is an error, not an `as usize` wrap (sqlite parity).
        let err =
            session_embedding_from_parts(Some("bge_m3".into()), None, Some(-1), sid).unwrap_err();
        assert!(err.to_string().contains("negative embedding_dim"), "{err}");
        // Well-formed rows still parse.
        let got = session_embedding_from_parts(
            Some("bge_m3".into()),
            Some("BAAI/bge-m3".into()),
            Some(1024),
            sid,
        )
        .unwrap();
        let stored = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("BAAI/bge-m3".into()),
            dim: 1024,
        };
        assert_eq!(got, Some(stored.clone()));
        let live = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("renamed-bge-m3.gguf".into()),
            dim: 1024,
        };
        let err =
            crate::resolve::assert_session_embedding_compatible(Some(&stored), &live).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("BAAI/bge-m3"), "{text}");
        assert!(text.contains("renamed-bge-m3.gguf"), "{text}");
        assert!(text.contains("--allow-embedding-mismatch"), "{text}");
        assert_eq!(
            session_embedding_from_parts(None, None, None, sid).unwrap(),
            None
        );
    }
}

// ---------------------------------------------------------------------------
// Live conformance (feature-gated; honest skips without LAMBO_COCKROACH_DSN)
// ---------------------------------------------------------------------------

/// Runs under `cargo test --features store-cockroach` (with `fixtures`, which is in the
/// default set). The two live tests are `#[ignore]`d, so a run without
/// `LAMBO_COCKROACH_DSN` reports them as **ignored** — never a silent, skip-as-green
/// `ok`. To actually run them: `cargo test --features store-cockroach -- --ignored`.
/// With `LAMBO_REQUIRE_LIVE=1` (evidence capture) a missing DSN is a hard failure:
/// [`dsn_or_skip`] panics and [`live_dsn_gate_fails_loudly_when_required`] fails even
/// when the ignored tests did not run. The DSN is read from the environment only and
/// never printed.
#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]
mod conformance {
    use super::*;
    use crate::fixtures::{load_mutation_batch, load_snapshot};
    use crate::types::{AgentId, ConceptType, Interaction, Node};
    use crate::MemoryStore;
    use chrono::TimeZone;
    use std::env;
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    fn dsn() -> Option<String> {
        env::var("LAMBO_COCKROACH_DSN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn missing_dsn_is_fatal(require_live: bool, require_vector_index: bool) -> bool {
        require_live || require_vector_index
    }

    /// Returns the DSN when present. Without one: under `LAMBO_REQUIRE_LIVE=1` or
    /// `LAMBO_REQUIRE_VECTOR_INDEX=1` this PANICS (an evidence-captured live run must
    /// never silently skip); otherwise it prints a skip notice and returns None.
    /// Callers are `#[ignore]`d, so a missing DSN surfaces as an ignored test in the
    /// default run, not a false `ok`.
    fn dsn_or_skip(test: &str) -> Option<String> {
        match dsn() {
            Some(d) => Some(d),
            None => {
                let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
                let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
                if missing_dsn_is_fatal(require_live, require_vector_index) {
                    panic!(
                        "{test}: LAMBO_COCKROACH_DSN is unset but a required-live flag \
                         is set (LAMBO_REQUIRE_LIVE={require_live}, \
                         LAMBO_REQUIRE_VECTOR_INDEX={require_vector_index}) — refusing \
                         to skip a live cockroach test"
                    );
                }
                eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
                None
            }
        }
    }

    /// Non-ignored honesty gate: the two live tests above are `#[ignore]`d, so the
    /// default run reports them as ignored instead of passing while skipping. With
    /// `LAMBO_REQUIRE_LIVE=1` a missing DSN must fail loudly — this gate fails even
    /// though the ignored tests themselves did not run.
    #[test]
    fn live_dsn_gate_fails_loudly_when_required() {
        let require_live = env::var_os("LAMBO_REQUIRE_LIVE").is_some();
        let require_vector_index = env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_some();
        if !missing_dsn_is_fatal(require_live, require_vector_index) {
            return;
        }
        assert!(
            dsn().is_some(),
            "LAMBO_REQUIRE_LIVE=1 or LAMBO_REQUIRE_VECTOR_INDEX=1 requires \
             LAMBO_COCKROACH_DSN: live cockroach tests must not be silently skipped \
             (run with -- --ignored and a real DSN)"
        );
    }

    #[test]
    fn vector_index_requirement_makes_missing_dsn_fatal() {
        assert!(!missing_dsn_is_fatal(false, false));
        assert!(missing_dsn_is_fatal(true, false));
        assert!(missing_dsn_is_fatal(false, true));
        assert!(missing_dsn_is_fatal(true, true));
    }

    fn cfg(dsn: String) -> StoreConfig {
        StoreConfig {
            kind: crate::store::StoreKind::Cockroach,
            dsn: Some(dsn),
            path: None,
            // Cockroach parses its width out of `VECTOR(n)`; the pin is for
            // width-agnostic adapters, so the conformance suite carries none.
            vector_dim: None,
        }
    }

    /// Fresh store for the suite. All checks run inside ONE `#[tokio::test]`, so the
    /// pool is created and every connection is used on the same (single-test) Tokio
    /// runtime — connections never cross runtimes, which avoids both "pool timed out
    /// while waiting for an open connection" (one pool, ≤ `MAX_POOL_CONNECTIONS` conns)
    /// and "A Tokio 1.x context was found, but it is being shutdown" (a pooled
    /// connection registered with a dead per-test runtime).
    fn new_store(dsn: &str) -> CockroachStore {
        CockroachStore::new(cfg(dsn.to_string())).unwrap()
    }

    /// T8.6 (live): the store-enforced single-writer lease across two **separate
    /// pools** (the cross-process shape — each pool is an independent set of
    /// connections, exactly what two processes have). One acquires, the other is
    /// refused fail-closed and told the holder; after a release the second wins;
    /// an unreleased lease is reclaimable only after its TTL.
    ///
    /// `#[ignore]`d like every live cockroach test, so a run without
    /// `LAMBO_COCKROACH_DSN` reports it as ignored, never a skip-as-green. Run:
    /// `cargo test --features store-cockroach,fixtures -- --ignored`.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn single_writer_lease_is_enforced_across_pools() {
        let Some(dsn) = dsn_or_skip("single_writer_lease_is_enforced_across_pools") else {
            return;
        };
        use crate::store::lease::{LeaseHolder, LeaseOutcome};

        let store_a = new_store(&dsn);
        store_a.init_schema().await.expect("init_schema");
        let store_b = new_store(&dsn);

        // Unique session per run so a shared cluster never cross-contaminates.
        let sid = SessionId::from(format!("t8.6-lease-{}", Uuid::new_v4()));
        let a = LeaseHolder {
            endpoint: None,
            agent: AgentId::new("proc-a"),
            pid: 111,
            host: "host-a".into(),
        };
        let b = LeaseHolder {
            endpoint: None,
            agent: AgentId::new("proc-b"),
            pid: 222,
            host: "host-b".into(),
        };
        let ttl = Duration::from_secs(30);

        // Clean any leftover row from a previous aborted run.
        let _ = store_a.release_lease(&sid, &a).await;

        assert!(store_a
            .acquire_lease(&sid, &a, ttl)
            .await
            .expect("A acquire")
            .is_acquired());

        match store_b
            .acquire_lease(&sid, &b, ttl)
            .await
            .expect("B acquire")
        {
            LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
            other => panic!("the second pool must be refused, got {other:?}"),
        }

        // Refresh keeps acquired_at.
        let LeaseOutcome::Acquired(first) = store_a.acquire_lease(&sid, &a, ttl).await.unwrap()
        else {
            panic!("A refresh");
        };
        let LeaseOutcome::Acquired(refreshed) = store_a.refresh_lease(&sid, &a, ttl).await.unwrap()
        else {
            panic!("A refresh 2");
        };
        assert_eq!(first.acquired_at, refreshed.acquired_at);

        // J2: the endpoint column, on the live cluster. A holds no endpoint
        // (it was built without one), so the row says so; republishing as a
        // reachable holder writes it, `read_lease` reads it back without
        // touching the lease, and a refresh does not blank it.
        assert_eq!(first.endpoint, None);
        assert_eq!(
            store_a
                .read_lease(&sid)
                .await
                .expect("read A")
                .unwrap()
                .endpoint,
            None
        );
        let a_hub = a.clone().reachable_at("/run/lambo/crdb.sock");
        let LeaseOutcome::Acquired(published) =
            store_a.acquire_lease(&sid, &a_hub, ttl).await.unwrap()
        else {
            panic!("A republish as a reachable holder");
        };
        assert_eq!(published.endpoint.as_deref(), Some("/run/lambo/crdb.sock"));
        assert_eq!(
            store_b
                .read_lease(&sid)
                .await
                .expect("B reads A's row")
                .unwrap()
                .endpoint
                .as_deref(),
            Some("/run/lambo/crdb.sock"),
            "a losing process must be able to read the holder's endpoint from \
             another pool — that read IS the proxy path"
        );
        let LeaseOutcome::Acquired(after_refresh) =
            store_a.refresh_lease(&sid, &a_hub, ttl).await.unwrap()
        else {
            panic!("A refresh 3");
        };
        assert_eq!(
            after_refresh.endpoint.as_deref(),
            Some("/run/lambo/crdb.sock")
        );

        // Release → B takes it.
        store_a.release_lease(&sid, &a).await.expect("A release");
        assert!(store_b
            .acquire_lease(&sid, &b, ttl)
            .await
            .expect("B re-acquire")
            .is_acquired());

        // Expiry-after-crash: B holds a short-TTL lease and never releases; A is
        // refused before the TTL and reclaims after it.
        let short = Duration::from_secs(2);
        store_b.release_lease(&sid, &b).await.ok();
        store_b
            .acquire_lease(&sid, &b, short)
            .await
            .expect("B short");
        assert!(matches!(
            store_a.acquire_lease(&sid, &a, ttl).await.unwrap(),
            LeaseOutcome::Held { .. }
        ));
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(store_a
            .acquire_lease(&sid, &a, ttl)
            .await
            .expect("A reclaim after expiry")
            .is_acquired());

        // Cleanup.
        store_a.release_lease(&sid, &a).await.ok();
    }

    /// T7.4: prove the ANN accuracy dial actually reaches the server **through
    /// sqlx**, not merely that it parses.
    ///
    /// This test exists because the obvious way to check it by hand is wrong:
    /// `PGOPTIONS=-c vector_search_beam_size=128 psql …` is silently IGNORED on
    /// this deployment (measured 2026-08-13: still reports 32, and a
    /// `statement_timeout` set the same way reports 0). Only the `options`
    /// **connection parameter** in the startup message is honoured — which is
    /// what `PgConnectOptions::options()` sets, and what `pool()` uses for both
    /// `statement_timeout` and this dial. A hand-check with `PGOPTIONS` would
    /// therefore "disprove" a setting that works perfectly.
    ///
    /// Also pins that `options()` APPENDS rather than replaces: sqlx 0.8 builds
    /// one space-joined `-c k=v` string, so adding the beam size must not drop
    /// the STORE-2 `statement_timeout` bound.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn vector_beam_size_reaches_the_server_and_keeps_statement_timeout() {
        let Some(dsn) = dsn_or_skip("vector_beam_size_reaches_the_server") else {
            return;
        };
        // Build the options under the env lock and RELEASE it before any await
        // (spec §6.4 / clippy::await_holding_lock). This is exactly why
        // `connect_options` is synchronous.
        let options = {
            let _g = crate::test_util::env_lock();
            let restore = env::var(VECTOR_BEAM_SIZE_ENV).ok();
            env::set_var(VECTOR_BEAM_SIZE_ENV, "128");
            // Same normalization `CockroachStore::new` applies: sqlx + rustls
            // cannot open libpq's `sslrootcert=system`, and `connect_options`
            // is fed `self.dsn`, which is already rewritten.
            let built = CockroachStore::connect_options(&dsn_for_rustls(&dsn));
            match restore {
                Some(v) => env::set_var(VECTOR_BEAM_SIZE_ENV, v),
                None => env::remove_var(VECTOR_BEAM_SIZE_ENV),
            }
            built.unwrap()
        };

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(options);
        let beam: String = sqlx::query_scalar("SHOW vector_search_beam_size")
            .fetch_one(&pool)
            .await
            .unwrap();
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(
            beam, "128",
            "explicit LAMBO_VECTOR_BEAM_SIZE did not reach the server"
        );
        assert_ne!(
            timeout, "0",
            "adding the beam-size option dropped the STORE-2 statement_timeout \
             — options() must append, not replace"
        );
    }

    /// **J3-R2R-3, live.** The Cockroach dialect half of the column preflight:
    /// on a real cluster, `init_schema`, a passing preflight, then a required
    /// column temporarily renamed away (the older-build shape) refusing by
    /// table and column name through the `information_schema.columns` path, and
    /// the rename reverted so the shared cluster is left exactly as found.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn column_preflight_refuses_a_missing_column_live() {
        let Some(dsn) = dsn_or_skip("column_preflight_refuses_a_missing_column_live") else {
            return;
        };
        let store = new_store(&dsn);
        store
            .init_schema()
            .await
            .expect("init_schema on live cluster");
        store
            .preflight_schema()
            .await
            .expect("a provisioned live cluster must pass the column preflight");
        let pool = store.pool().await.expect("pool");
        sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id TO chunk_group_id_x")
            .execute(pool)
            .await
            .expect("rename a required column away (older-build shape)");
        let err = store
            .preflight_schema()
            .await
            .expect_err("a live cluster missing a required column must not pass")
            .to_string();
        assert!(err.contains("concepts"), "names the table: {err}");
        assert!(err.contains("chunk_group_id"), "names the column: {err}");
        assert!(err.contains("lambo provision"), "actionable: {err}");
        sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id_x TO chunk_group_id")
            .execute(pool)
            .await
            .expect("rename the column back — the shared cluster is left as found");
        store
            .preflight_schema()
            .await
            .expect("passes again after the rename is reverted");
    }

    fn embed(seed: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..1024)
            .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        v
    }

    fn plant_interaction(sid: &SessionId, id: NodeId, ts: DateTime<Utc>) -> Mutation {
        Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                id,
                session_id: sid.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("seed".into()),
                previous_id: None,
                created_at: ts,
                event_time: None,
            }),
        }
    }

    fn plant_concept(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        ts: DateTime<Utc>,
        embedding: Option<Vec<f32>>,
    ) -> Mutation {
        plant_concept_full(
            sid,
            id,
            origin,
            content,
            &content.to_lowercase(),
            ConceptType::Entity,
            ts,
            embedding,
            None,
        )
    }

    /// Full-shape concept planter: verbatim canonical_key, concept type, and
    /// chunk_group_id — used by the legal-demote (R2), chunk_group_id round-trip,
    /// and mixed-case keyword checks.
    // Test helper: every parameter is a distinct planter field (same precedent as
    // derive.rs resolve_concept) — a params struct would obscure the call sites.
    #[allow(clippy::too_many_arguments)]
    fn plant_concept_full(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        canonical_key: &str,
        concept_type: ConceptType,
        ts: DateTime<Utc>,
        embedding: Option<Vec<f32>>,
        chunk_group_id: Option<String>,
    ) -> Mutation {
        Mutation::UpsertNode {
            node: Node::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: content.into(),
                canonical_key: canonical_key.into(),
                concept_type,
                origin_interaction: origin,
                origin_agent: AgentId::from("agent-a"),
                created_at: ts,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding,
                chunk_group_id,
                human_confirmed: 0,
            }),
        }
    }

    fn plant_edge(
        sid: &SessionId,
        source: NodeId,
        target: NodeId,
        edge_type: EdgeType,
        ts: DateTime<Utc>,
    ) -> Mutation {
        Mutation::UpsertEdge {
            edge: Edge {
                id: NodeId::new(),
                session_id: sid.clone(),
                source,
                target,
                edge_type,
                weight: 1.0,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
                event_time: None,
            },
        }
    }

    fn sorted_snap_parts(snap: &GraphSnapshot) -> (Vec<NodeId>, Vec<NodeId>, Vec<NodeId>) {
        let mut ii: Vec<NodeId> = snap.interactions.iter().map(|i| i.id).collect();
        let mut cc: Vec<NodeId> = snap.concepts.iter().map(|c| c.id).collect();
        let mut ee: Vec<NodeId> = snap.edges.iter().map(|e| e.id).collect();
        ii.sort_by_key(|n| n.0);
        cc.sort_by_key(|n| n.0);
        ee.sort_by_key(|n| n.0);
        (ii, cc, ee)
    }

    async fn check_init_schema_idempotent(store: &CockroachStore) {
        store.init_schema().await.unwrap();
        store.init_schema().await.unwrap();
    }

    /// No Tokio needed: `build_store` constructs the adapter without creating the pool
    /// (lazy OnceCell), so this runs as a plain sync test. The real DSN also exercises
    /// the rustls rewrite + parse validation at construction. `#[ignore]`d: without
    /// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green.
    #[test]
    #[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
    fn build_store_returns_working_adapter() {
        let Some(dsn) = dsn_or_skip("build_store_returns_working_adapter") else {
            return;
        };
        let s = crate::store::build_store(cfg(dsn)).unwrap();
        assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
        assert_eq!(s.vector_dimensions(), Some(1024));
    }

    async fn check_flush_mutations_batch_roundtrip(store: &CockroachStore) {
        let batch = load_mutation_batch("mutations-batch").unwrap();
        let sid = SessionId::from("session-mutations");
        store.flush(&batch, None).await.unwrap();

        // Direct snapshot read-back.
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.interactions.len(), 2, "both interactions survive");
        assert_eq!(snap.concepts.len(), 1, "deleted concept removed");
        assert_eq!(snap.concepts[0].content, "kept concept");
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Candidate,
            "canonization transition applied"
        );
        assert_eq!(snap.edges.len(), 1, "delete_edge + incident-edge cleanup");
        assert_eq!(
            snap.edges[0].id,
            NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007052").unwrap()),
            "only the Derives edge survives"
        );
        assert_eq!(snap.canonization_events.len(), 1);
        assert_eq!(
            snap.canonization_events[0].node_id,
            NodeId(Uuid::from_str("f0000000-0000-4000-8000-000000007002").unwrap())
        );
        // NOTE: this fixture's final state is intentionally NOT a legal §5.7 graph — it
        // deletes the Temporal edge between the two interactions, so the loaded snapshot
        // cannot be materialized via Graph::from_snapshot (would fail the Temporal-edge
        // invariant). Snapshot-level round-trip is the correct conformance here; graph
        // materialization of legal batches is covered by load.rs tests.
    }

    async fn check_load_missing_session_is_session_not_found(store: &CockroachStore) {
        let err = store
            .load_session(&SessionId::from(format!("no-such-{}", Uuid::new_v4())))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    async fn check_vector_write_and_candidates_top1(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-vector-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let ts = Utc::now();
        let probe = embed(0.17);
        let contract = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store.vector_dim,
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        Mutation::SetEmbedding {
                            session_id: sid.clone(),
                            embedding: Some(contract.clone()),
                        },
                        plant_interaction(&sid, i1, ts),
                        plant_concept(&sid, a, i1, "alpha concept", ts, Some(probe.clone())),
                        plant_concept(&sid, b, i1, "beta concept", ts, Some(embed(0.5))),
                    ],
                },
                None,
            )
            .await
            .unwrap();

        let hits = store
            .vector_candidates_checked(&sid, &probe, &contract, 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].item, a, "identical embedding must rank first");
        assert!(
            (hits[0].score - 1.0).abs() < 1e-4,
            "score {}",
            hits[0].score
        );
        assert!(hits[0].score > hits[1].score);

        // Round-trip the stored vector through load_session.
        let snap = store.load_session(&sid).await.unwrap();
        let back = snap
            .concepts
            .iter()
            .find(|c| c.id == a)
            .unwrap()
            .embedding
            .as_ref()
            .unwrap();
        assert_eq!(back.len(), 1024);
        let max_diff = probe
            .iter()
            .zip(back.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-4, "vector round-trip max diff {max_diff}");
    }

    async fn check_vector_candidates_are_session_scoped(store: &CockroachStore) {
        // DECISION D1: the SQL queries GLOBALLY, so a closer foreign-session concept is
        // in the raw top-k — the Rust session filter must drop it, never return it.
        // Two near-paraphrase concepts in session A should retrieve each other via the
        // index while staying session-scoped.
        let sid_a = SessionId::from(format!("conformance-vecscope-a-{}", Uuid::new_v4()));
        let sid_b = SessionId::from(format!("conformance-vecscope-b-{}", Uuid::new_v4()));
        let (i1, i2) = (NodeId::new(), NodeId::new());
        let (a, c, b) = (NodeId::new(), NodeId::new(), NodeId::new());
        let ts = Utc::now();
        let probe = embed(0.11);
        let contract = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store.vector_dim,
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        Mutation::SetEmbedding {
                            session_id: sid_a.clone(),
                            embedding: Some(contract.clone()),
                        },
                        plant_interaction(&sid_a, i1, ts),
                        plant_concept(&sid_a, a, i1, "register user", ts, Some(probe.clone())),
                        plant_concept(&sid_a, c, i1, "create account", ts, Some(embed(0.115))),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        // Foreign session B holds a concept whose vector is EXACTLY the probe — the
        // globally-closest row — so it would rank first in the raw query.
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        Mutation::SetEmbedding {
                            session_id: sid_b.clone(),
                            embedding: Some(contract.clone()),
                        },
                        plant_interaction(&sid_b, i2, ts),
                        plant_concept(&sid_b, b, i2, "foreign closer", ts, Some(probe.clone())),
                    ],
                },
                None,
            )
            .await
            .unwrap();

        let hits = store
            .vector_candidates_checked(&sid_a, &probe, &contract, 10)
            .await
            .unwrap();
        let items: Vec<_> = hits.iter().map(|h| h.item).collect();
        assert!(
            !items.contains(&b),
            "foreign-session node {b} leaked into results: {items:?}"
        );
        assert!(
            items.contains(&a) && items.contains(&c),
            "near paraphrases should retrieve each other; got {items:?}"
        );
        assert_eq!(
            hits[0].item, a,
            "own exact candidate should rank first in-session; got {items:?}"
        );
    }

    async fn check_vector_explain_is_global_topk(store: &CockroachStore) {
        // DECISION D1 shape on the live plan: the query is GLOBAL (no session predicate),
        // so the plan must NOT scan the anti-pattern `concepts_session_id_canonical_key_key`
        // (the T0.3-spike shape that bypassed the vector index). The plan is an ordered
        // top-k over the whole table; whether the optimizer accelerates that top-k with
        // the vector index (`vector search`) is asserted separately by the standalone
        // `vector_explain_camera_proof` gate.
        //
        // T7.4 correction (2026-08-13): this comment used to say that gate was
        // "PENDING where the optimizer scans a small table". That was wrong on both
        // counts — table size was never the cause. The proof failed because it asserted
        // the spaced `vector search` against `EXPLAIN (OPT, VERBOSE)`, which spells the
        // operator `vector-search`, and because a NON-partial vector index cannot imply
        // the query's `embedding IS NOT NULL` predicate, forcing a FULL SCAN. With the
        // index partial (migrations/cockroach/001_init.sql) the proof is green.
        //
        // T7.3 remediation (planner-variance hardening): EXPLAIN with a LITERAL
        // `LIMIT 5` (not a parameterized `LIMIT $2`) to match the captured T0.3
        // evidence, which reproduced a `top-k` node under the literal shape. With a
        // placeholder limit the optimizer MAY fall back to a `limit` + `sort` plan
        // instead — the same correct global-ordered semantics, different node name.
        // So the positive assertion accepts EITHER a `top-k` or a `limit` ordering
        // construct; the assertion that MUST always hold — and is the DECISION D1
        // non-negotiable — is that the plan does NOT reference the session-filtered
        // anti-pattern index. This keeps the gate green against planner variance
        // without weakening the no-anti-pattern guarantee.
        let pool = store.pool().await.unwrap();
        let probe = encode_vector(&embed(0.5)).unwrap();
        let rows = sqlx::query(
            "EXPLAIN (OPT, VERBOSE) \
             SELECT id, session_id, embedding <-> $1::VECTOR AS dist \
             FROM concepts WHERE embedding IS NOT NULL ORDER BY dist ASC LIMIT 5",
        )
        .bind(&probe)
        .fetch_all(pool)
        .await
        .map_err(backend)
        .unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, usize>(0).map_err(backend))
            .collect::<Result<_, _>>()
            .unwrap();
        let text = plan.join("\n");
        assert!(
            text.contains("top-k") || text.contains("limit"),
            "EXPLAIN must be a global ordered top-k/limit query, got:\n{text}"
        );
        assert!(
            !text.contains("concepts_session_id_canonical_key_key"),
            "EXPLAIN must NOT scan the session-filtered index (DECISION D1 anti-pattern):\n{text}"
        );
    }

    async fn check_keyword_candidates_on_planted_concept(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-kw-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, Utc::now()),
                        plant_concept(&sid, c1, i1, "user schema", Utc::now(), None),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let hits = store
            .keyword_candidates(&sid, &["schema".into()], 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item, c1);
        assert_eq!(hits[0].score, 1.0);
        // Empty tokens match nothing (MemoryStore parity).
        assert!(store
            .keyword_candidates(&sid, &["   ".into()], 5)
            .await
            .unwrap()
            .is_empty());
        // Missing session -> SessionNotFound (MemoryStore parity).
        let err = store
            .keyword_candidates(&SessionId::from("nope"), &["schema".into()], 5)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::SessionNotFound(_)));
    }

    async fn check_keyword_mixed_case_ranks_like_memory_store(store: &CockroachStore) {
        // Regression (P3 review R1): the SQL predicate lowercases content/key, so the
        // score must fold case too. A mixed-case concept ("Register User") matched by
        // token "register" must score > 0 and rank exactly like MemoryStore — a
        // raw-`contains` score would give it 0.0 and sink it below "user schema".
        let sid = SessionId::from(format!("conformance-kwcase-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let mixed = NodeId::new();
        let lower = NodeId::new();
        let ts = Utc::now();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, ts),
                // Mixed-case content AND canonical_key — selected by the SQL's lower()
                // predicate, scored only if the score loop folds case.
                plant_concept_full(
                    &sid,
                    mixed,
                    i1,
                    "Register User",
                    "Register User",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
                plant_concept_full(
                    &sid,
                    lower,
                    i1,
                    "user schema",
                    "user schema",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
            ],
        };
        store.flush(&batch, None).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&batch, None).await.unwrap();

        let tokens: Vec<String> = vec!["register".into(), "user".into()];
        let crdb = store.keyword_candidates(&sid, &tokens, 5).await.unwrap();
        let mem_res = mem.keyword_candidates(&sid, &tokens, 5).await.unwrap();
        assert_eq!(
            crdb, mem_res,
            "mixed-case scoring must match MemoryStore exactly"
        );
        assert_eq!(crdb.len(), 2);
        assert_eq!(
            crdb[0].item, mixed,
            "Register User (2 hits) must rank above user schema (1 hit)"
        );
        assert_eq!(crdb[0].score, 2.0);
        assert_eq!(crdb[1].item, lower);
        assert_eq!(crdb[1].score, 1.0);
    }

    async fn check_legal_demote_flush_partial_index(store: &CockroachStore) {
        // R2 (P3 review): the schema's canonical-key unique index is PARTIAL
        // (`WHERE concept_type <> 'Observation'`, spec §4 errata / muse-spark M1-M2).
        // A legal demote (T2.5) writes Observations that share a canonical key
        // (identical sentences from different chunks); those must flush successfully
        // against the live store instead of colliding on the index.
        let sid = SessionId::from(format!("conformance-demote-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let o1 = NodeId::new();
        let o2 = NodeId::new();
        let ts = Utc::now();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, ts),
                        plant_concept_full(
                            &sid,
                            o1,
                            i1,
                            "identical sentence",
                            "identical sentence",
                            ConceptType::Observation,
                            ts,
                            None,
                            Some("chunk-1".into()),
                        ),
                        plant_concept_full(
                            &sid,
                            o2,
                            i1,
                            "identical sentence",
                            "identical sentence",
                            ConceptType::Observation,
                            ts,
                            None,
                            Some("chunk-1".into()),
                        ),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 2, "both demoted Observations survive");
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.concept_type == ConceptType::Observation));
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.canonical_key == "identical sentence"));
        assert!(snap
            .concepts
            .iter()
            .all(|c| c.chunk_group_id.as_deref() == Some("chunk-1")));

        // Negative lock on the same index: a duplicate-key NON-Observation must be
        // rejected (the RAM graph rejects it as an invariant; the store fails loudly).
        let e1 = NodeId::new();
        let e2 = NodeId::new();
        let bad = MutationBatch {
            mutations: vec![
                plant_concept_full(
                    &sid,
                    e1,
                    i1,
                    "dup key",
                    "dup key",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
                plant_concept_full(
                    &sid,
                    e2,
                    i1,
                    "dup key",
                    "dup key",
                    ConceptType::Entity,
                    ts,
                    None,
                    None,
                ),
            ],
        };
        assert!(
            store.flush(&bad, None).await.is_err(),
            "duplicate-key non-Observation must violate concepts_key_non_obs_idx"
        );
    }

    async fn check_chunk_group_id_survives_flush_load(store: &CockroachStore) {
        // T5.2 contract (schema persistence): flush→load must PRESERVE
        // chunk_group_id — the implementer's snapshot normalization to None is gone;
        // the round-trip now asserts survival.
        let sid = SessionId::from(format!("conformance-cgid-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let obs = NodeId::new();
        let plain = NodeId::new();
        let ts = Utc::now();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, ts),
                        plant_concept_full(
                            &sid,
                            obs,
                            i1,
                            "overflow sentence",
                            "overflow sentence",
                            ConceptType::Observation,
                            ts,
                            None,
                            Some("chunk-42".into()),
                        ),
                        plant_concept_full(
                            &sid,
                            plain,
                            i1,
                            "plain concept",
                            "plain concept",
                            ConceptType::Entity,
                            ts,
                            None,
                            None,
                        ),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        let obs_row = snap.concepts.iter().find(|c| c.id == obs).unwrap();
        assert_eq!(
            obs_row.chunk_group_id.as_deref(),
            Some("chunk-42"),
            "chunk_group_id must survive flush→load (T5.2 sibling co-retrieval key)"
        );
        let plain_row = snap.concepts.iter().find(|c| c.id == plain).unwrap();
        assert_eq!(
            plain_row.chunk_group_id, None,
            "NULL chunk_group_id stays None"
        );
    }

    async fn check_embedding_contract_read_and_flush_immunity(store: &CockroachStore) {
        // Embedding contract (S5-class snapshot metadata): seed — the PRODUCTION
        // full-snapshot path (STORE-1 remediation) — persists embedding_kind/model/dim,
        // and load_session materializes GraphSnapshot.embedding when present. A later
        // flush (which only ensures the session row; there is no session-metadata
        // Mutation kind) must NOT clobber the stamped contract.
        let sid = SessionId::from(format!("conformance-embed-{}", Uuid::new_v4()));
        store
            .seed(&GraphSnapshot {
                session_id: sid.clone(),
                embedding: Some(EmbeddingContract {
                    kind: "bge_m3".into(),
                    model: Some("BAAI/bge-m3".into()),
                    dim: 1024,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(
            snap.embedding,
            Some(EmbeddingContract {
                kind: "bge_m3".into(),
                model: Some("BAAI/bge-m3".into()),
                dim: 1024,
            }),
            "load_session must materialize the seeded embedding contract"
        );
        // A subsequent flush without a metadata mutation must not clobber the
        // seeded contract.
        let i1 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, Utc::now()),
                        plant_concept(&sid, NodeId::new(), i1, "seed concept", Utc::now(), None),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        let emb = snap
            .embedding
            .expect("embedding contract survives a later flush");
        assert_eq!(emb.kind, "bge_m3");
        assert_eq!(emb.model.as_deref(), Some("BAAI/bge-m3"));
        assert_eq!(emb.dim, 1024);
        assert_eq!(
            snap.concepts.len(),
            1,
            "flush content still lands alongside the contract"
        );
        // Session with no stamp reads back None.
        let plain_sid = SessionId::from(format!("conformance-embed-none-{}", Uuid::new_v4()));
        store
            .flush(
                &MutationBatch {
                    mutations: vec![plant_interaction(&plain_sid, NodeId::new(), Utc::now())],
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&plain_sid).await.unwrap();
        assert_eq!(snap.embedding, None, "unstamped session has no contract");
    }

    async fn check_seed_load_full_snapshot_roundtrip(store: &CockroachStore) {
        let snap = load_snapshot("session-rest-api").unwrap();
        store.seed(&snap).await.unwrap();
        let loaded = store.load_session(&snap.session_id).await.unwrap();

        let (li, lc, le) = sorted_snap_parts(&loaded);
        let (si, sc, se) = sorted_snap_parts(&snap);
        assert_eq!(li, si, "interactions round-trip");
        assert_eq!(lc, sc, "concepts round-trip");
        assert_eq!(le, se, "edges round-trip");

        // Field-level equality on representative rows.
        assert_eq!(loaded.session_id, snap.session_id);
        assert_eq!(loaded.root_goal, snap.root_goal);
        assert_eq!(loaded.created_at, snap.created_at);
        assert_eq!(loaded.closed_at, snap.closed_at);
        assert_eq!(
            loaded.synonyms.len(),
            snap.synonyms.len(),
            "synonym persisted via seed"
        );
        assert_eq!(loaded.synonyms[0].source_key, snap.synonyms[0].source_key);
        assert_eq!(
            loaded.synonyms[0].canonical_key,
            snap.synonyms[0].canonical_key
        );
        assert!(loaded.reservations.is_empty());
        assert!(loaded.canonization_events.is_empty());

        // Full deep-equality of the whole graph (fixture order is id-ordered).
        let mut a = loaded.concepts.clone();
        let mut b = snap.concepts.clone();
        a.sort_by_key(|c| c.id.0);
        b.sort_by_key(|c| c.id.0);
        assert_eq!(a, b, "concept rows deep-equal (incl. timestamps)");
        let mut ai = loaded.interactions.clone();
        let mut bi = snap.interactions.clone();
        ai.sort_by_key(|i| i.id.0);
        bi.sort_by_key(|i| i.id.0);
        assert_eq!(ai, bi);
        let mut ae = loaded.edges.clone();
        let mut be = snap.edges.clone();
        ae.sort_by_key(|e| e.id.0);
        be.sort_by_key(|e| e.id.0);
        assert_eq!(ae, be);
    }

    /// T3.6 matrix for ONE fixture: seed it into the live Cockroach store AND a
    /// fresh MemoryStore, then assert EVERY node (concepts + interactions) ×
    /// min-age {0, 3600s} × both queries answers EXACTLY like MemoryStore's
    /// naive scan on the same snapshot. Seeding is an idempotent upsert, so
    /// re-runs against a persistent cluster converge. Returns the number of
    /// equality assertions performed (2 per node × age cell).
    async fn check_fixture_structural_agreement(store: &CockroachStore, fixture: &str) -> usize {
        let snap = load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();
        store.seed(&snap).await.unwrap();

        let mem = {
            let m = MemoryStore::new();
            m.seed(snap.clone()).unwrap();
            Arc::new(m)
        };

        let node_ids: Vec<NodeId> = snap
            .concepts
            .iter()
            .map(|c| c.id)
            .chain(snap.interactions.iter().map(|i| i.id))
            .collect();
        let ages = [Duration::ZERO, Duration::from_secs(3600)];
        let mut assertions = 0usize;
        for node in &node_ids {
            for age in ages {
                let mem_br = mem
                    .blast_radius(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let crdb_br = store
                    .blast_radius(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(mem_br, crdb_br, "blast_radius({fixture}, {node}, {age:?})");
                let mem_span = mem
                    .interaction_span(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                let crdb_span = store
                    .interaction_span(&sid, *node, age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(
                    mem_span, crdb_span,
                    "interaction_span({fixture}, {node}, {age:?}): mem={mem_span:?} crdb={crdb_span:?}"
                );
                assertions += 2;
            }
        }

        // Deterministic anchors on rest-api (independent of the oracle): eight
        // concepts depend on the Canonical hub 1001 exclusively; span = 6
        // distinct interactions over 25 of 55 minutes.
        if fixture == "session-rest-api" {
            let hub: NodeId = snap
                .concepts
                .iter()
                .find(|c| c.id.0.to_string().ends_with("001001"))
                .unwrap()
                .id;
            assert_eq!(
                store
                    .blast_radius(&sid, hub, Duration::ZERO, Utc::now())
                    .await
                    .unwrap(),
                8,
                "hub 1001 blast radius anchor"
            );
            let span = store
                .interaction_span(&sid, hub, Duration::ZERO, Utc::now())
                .await
                .unwrap();
            assert_eq!(span.distinct, 6);
            assert!(
                (span.coverage - 25.0 / 55.0).abs() < 1e-9,
                "hub 1001 span anchor: {span:?}"
            );
        }
        assertions
    }

    /// T3.6 acceptance: the three-way agreement matrix on BOTH fixture graphs —
    /// `session-rest-api` (22 concepts incl. Canonical hub 1001, Venerable
    /// 1012, D1–D8 orphans, P1/P2 peers) and `session-drift` (9 concepts) —
    /// every node × min-age {0, 3600s}, blast_radius + interaction_span
    /// (distinct AND coverage) exactly equal to MemoryStore.
    async fn check_structural_queries_agree_with_memory_store(store: &CockroachStore) {
        let mut total_assertions = 0usize;
        for fixture in ["session-rest-api", "session-drift"] {
            total_assertions += check_fixture_structural_agreement(store, fixture).await;
        }
        // Lock the matrix dimensions like the sqlite suite (round-1 review F2):
        // rest-api 34 nodes (22 concepts + 12 interactions) + drift 11 nodes
        // (9 + 2) = 45 nodes; 2 ages; 2 queries each -> 180 equality assertions.
        // A future narrowing of either matrix loop silently shrinks coverage,
        // so the count must be verified, not just implied by the loop shape.
        assert_eq!(total_assertions, 180, "matrix dimensions drifted");
    }

    async fn check_structural_queries_age_filter_agrees(store: &CockroachStore) {
        // T3.6 matrix + round-1 review F1 remediation: the span's TWO timestamp
        // gates are discriminated behaviorally, not just textually —
        // * e-gate: the fresh edge's source carries a DISTINCT origin
        //   interaction (`i2`), so the span set genuinely shrinks at
        //   min_age = 3600s: `span(orphan).distinct` is 2 at min-age 0 and 1 at
        //   1h (before the fix every origin was `i1`, so the span was identical
        //   with or without either gate);
        // * i-gate probe: an AGED edge (`probe_src -> probe_victim`) whose
        //   origin interaction `i3` is FRESH (created after the 1h cutoff) must
        //   be excluded from the span — `span(probe_victim).distinct` is 1 at
        //   min-age 0 and 0 at 1h.
        // blast_radius is origin-agnostic by contrast (the i-gate is span-only,
        // matching MemoryStore).
        let sid = SessionId::from(format!("conformance-age-{}", Uuid::new_v4()));
        let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let now = Utc::now();
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        let i3 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let other = NodeId::new();
        let probe_src = NodeId::new();
        let probe_victim = NodeId::new();

        // Base: aged graph — pillar -> orphan (aged edge, aged origin i1) and
        // probe_src -> probe_victim (aged edge, FRESH origin i3: the i-gate
        // probe). `other`'s origin is DISTINCT i2 — the fresh edge's source.
        let base = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, old_ts),
                plant_interaction(&sid, i2, old_ts),
                plant_interaction(&sid, i3, now),
                plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
                plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
                plant_concept(&sid, other, i2, "other", old_ts, None),
                plant_concept(&sid, probe_src, i3, "probe-src", old_ts, None),
                plant_concept(&sid, probe_victim, i1, "probe-victim", old_ts, None),
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
                plant_edge(&sid, probe_src, probe_victim, EdgeType::Dependency, old_ts),
            ],
        };
        store.flush(&base, None).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&base, None).await.unwrap();
        // Then a genuinely FRESH other -> orphan dependency (created now).
        let fresh = MutationBatch {
            mutations: vec![plant_edge(&sid, other, orphan, EdgeType::Dependency, now)],
        };
        store.flush(&fresh, None).await.unwrap();
        mem.flush(&fresh, None).await.unwrap();

        let one_hour = Duration::from_secs(3600);
        // Aged-vs-fresh edge interaction: EVERY node × both cutoffs must agree
        // with MemoryStore exactly (the aged pillar -> orphan edge vs the
        // freshly created other -> orphan edge, plus the i-gate probe).
        for node in [pillar, orphan, other, probe_src, probe_victim, i1, i2, i3] {
            for min_age in [Duration::ZERO, one_hour] {
                let mem_br = mem
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let crdb_br = store
                    .blast_radius(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(mem_br, crdb_br, "blast_radius({node}, {min_age:?})");
                let mem_span = mem
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                let crdb_span = store
                    .interaction_span(&sid, node, min_age, Utc::now())
                    .await
                    .unwrap();
                assert_eq!(
                    mem_span, crdb_span,
                    "interaction_span({node}, {min_age:?}): mem={mem_span:?} crdb={crdb_span:?}"
                );
            }
        }
        // e-gate discrimination on the SPAN (independent of the oracle):
        // `other`'s DISTINCT origin i2 counts at min-age 0 and must vanish at
        // 1h when the fresh edge is filtered.
        assert_eq!(
            store
                .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
                .await
                .unwrap()
                .distinct,
            2,
            "e-gate: fresh edge's distinct origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, orphan, one_hour, Utc::now())
                .await
                .unwrap()
                .distinct,
            1,
            "e-gate: fresh edge's distinct origin filtered at min_age=1h"
        );
        // i-gate probe: the AGED probe_src -> probe_victim edge's origin i3 is
        // FRESH, so it is in the span at min-age 0 and must be excluded at 1h.
        assert_eq!(
            store
                .interaction_span(&sid, probe_victim, Duration::ZERO, Utc::now())
                .await
                .unwrap()
                .distinct,
            1,
            "i-gate: fresh origin counts at min_age=0"
        );
        assert_eq!(
            store
                .interaction_span(&sid, probe_victim, one_hour, Utc::now())
                .await
                .unwrap()
                .distinct,
            0,
            "i-gate: aged edge with fresh origin excluded at min_age=1h"
        );
        // blast_radius is origin-agnostic: the aged probe edge counts at 1h
        // even though its origin is fresh (span-only i-gate, MemoryStore parity).
        assert_eq!(
            store
                .blast_radius(&sid, probe_src, one_hour, Utc::now())
                .await
                .unwrap(),
            1,
            "blast_radius ignores origin age"
        );
        // The age filter is doing real work for blast_radius: with min_age=0 the
        // fresh edge un-orphans; with min_age=1h it is filtered and the orphan
        // still counts.
        assert_eq!(
            store
                .blast_radius(&sid, pillar, Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            0,
            "fresh edge counts at min_age=0"
        );
        assert_eq!(
            store
                .blast_radius(&sid, pillar, one_hour, Utc::now())
                .await
                .unwrap(),
            1,
            "fresh edge filtered at min_age=1h"
        );
    }

    /// §4.1 errata probe (T3.6): mirror of MemoryStore's
    /// `blast_radius_ignores_provenance_derives_edges` against the live SQL
    /// adapter. §5.7 requires every concept to carry a `Derives` edge
    /// (interaction → concept); if blast_radius counted that inbound edge as
    /// "another source", every concept would look non-orphaned and Stage-3
    /// blast radius would collapse to ~0. Cockroach must ignore provenance
    /// (`Derives`/`Temporal`) edges exactly like MemoryStore — never
    /// un-orphaning a concept through them.
    async fn check_structural_queries_errata_derives_probe(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-errata-{}", Uuid::new_v4()));
        let old_ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        let alone = NodeId::new();
        let batch = MutationBatch {
            mutations: vec![
                plant_interaction(&sid, i1, old_ts),
                plant_concept(&sid, pillar, i1, "pillar", old_ts, None),
                plant_concept(&sid, orphan, i1, "orphan", old_ts, None),
                plant_concept(&sid, alone, i1, "alone", old_ts, None),
                // pillar -> orphan (Dependency): the only structural inbound.
                plant_edge(&sid, pillar, orphan, EdgeType::Dependency, old_ts),
                // orphan ALSO has the mandatory §5.7 Derives from its origin
                // interaction — counting it would un-orphan orphan.
                plant_edge(&sid, i1, orphan, EdgeType::Derives, old_ts),
                // alone has ONLY the Derives provenance: never an orphan of
                // anyone (no structural inbound edge exists at all).
                plant_edge(&sid, i1, alone, EdgeType::Derives, old_ts),
            ],
        };
        store.flush(&batch, None).await.unwrap();
        let mem = MemoryStore::new();
        mem.flush(&batch, None).await.unwrap();

        for min_age in [Duration::ZERO, Duration::from_secs(3600)] {
            let want = mem
                .blast_radius(&sid, pillar, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(want, 1, "oracle sanity: Derives must not un-orphan");
            let got = store
                .blast_radius(&sid, pillar, min_age, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                got, want,
                "Cockroach must ignore provenance Derives exactly like MemoryStore (min_age {min_age:?})"
            );
        }
    }

    /// F1: a single-interaction session (temporal extent is one point) with a
    /// supported inbound dependency reports coverage 1.0, not 0.0 — parity
    /// with the MemoryStore fix (canonization Stage 2 in short sessions).
    async fn check_interaction_span_single_point_session_coverage(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-span-single-{}", Uuid::new_v4()));
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap();
        let i1 = NodeId::new();
        let pillar = NodeId::new();
        let orphan = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, ts),
                        plant_concept(&sid, pillar, i1, "pillar", ts, None),
                        plant_concept(&sid, orphan, i1, "orphan", ts, None),
                        plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
                    ],
                },
                None,
            )
            .await
            .unwrap();

        let mem = MemoryStore::new();
        mem.flush(
            &MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i1, ts),
                    plant_concept(&sid, pillar, i1, "pillar", ts, None),
                    plant_concept(&sid, orphan, i1, "orphan", ts, None),
                    plant_edge(&sid, pillar, orphan, EdgeType::Dependency, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();

        let crdb_span = store
            .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        let mem_span = mem
            .interaction_span(&sid, orphan, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            crdb_span, mem_span,
            "three-way parity on the single-point session"
        );
        assert_eq!(crdb_span.distinct, 1);
        assert_eq!(crdb_span.coverage, 1.0, "F1: {crdb_span:?}");

        // Unsupported target: no inbound structural edges -> 0.0 on both.
        let empty_crdb = store
            .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        let empty_mem = mem
            .interaction_span(&sid, pillar, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(empty_crdb, empty_mem);
        assert_eq!(empty_crdb.distinct, 0);
        assert_eq!(empty_crdb.coverage, 0.0);
    }

    async fn check_record_canonization_appends_and_is_idempotent(store: &CockroachStore) {
        let sid = SessionId::from(format!("conformance-canon-{}", Uuid::new_v4()));
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, Utc::now()),
                        plant_concept(&sid, c1, i1, "pillar", Utc::now(), None),
                    ],
                },
                None,
            )
            .await
            .unwrap();

        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: c1,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Venerable,
            blast_radius: Some(9),
            last_demotion_time: None,
            occurred_at: Utc::now(),
        };
        store.record_canonization(&event, None).await.unwrap();
        // Same event re-recorded (retried flush) must not duplicate the audit row.
        store.record_canonization(&event, None).await.unwrap();

        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.canonization_events.len(), 1, "idempotent append");
        assert_eq!(
            snap.canonization_events[0].to_status,
            CanonizationStatus::Venerable
        );
        assert_eq!(snap.canonization_events[0].blast_radius, Some(9));
        assert_eq!(
            snap.concepts[0].canonization_status,
            CanonizationStatus::Venerable
        );
        assert_eq!(snap.concepts[0].blast_radius, Some(9));

        // Missing concept -> NotFound (MemoryStore parity).
        let ghost = CanonizationEvent {
            id: NodeId::new(),
            session_id: sid.clone(),
            node_id: NodeId::new(),
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Canonical,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: Utc::now(),
        };
        let err = store.record_canonization(&ghost, None).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    async fn check_corrupt_contract_row_load_errors(store: &CockroachStore) {
        // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
        // (manufactured here via DIRECT SQL — the store's write path can never
        // produce it) must make load_session error like sqlite, never return a
        // silent `embedding: None`. The offline unit test
        // `session_embedding_xor_corruption_errors_not_silent_none` covers the
        // same arms without a cluster; this check proves the end-to-end read path.
        let sid = SessionId::from(format!("conformance-store7-a-{}", Uuid::new_v4()));
        let pool = store.pool().await.unwrap();
        sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
            .bind(sid.0.as_str())
            .execute(pool)
            .await
            .unwrap();
        let err = store.load_session(&sid).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding_dim without embedding_kind"),
            "dim-without-kind row must error, not silently load: {err}"
        );

        // Mirror image: kind set, dim NULL.
        let sid2 = SessionId::from(format!("conformance-store7-b-{}", Uuid::new_v4()));
        sqlx::query("INSERT INTO sessions (session_id, embedding_kind) VALUES ($1, 'bge_m3')")
            .bind(sid2.0.as_str())
            .execute(pool)
            .await
            .unwrap();
        let err = store.load_session(&sid2).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding_kind without embedding_dim"),
            "{err}"
        );
    }

    async fn check_unstamped_vector_candidates_are_empty_until_contract_commit(
        store: &CockroachStore,
    ) {
        let probe = embed(0.23);
        let expected = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store.vector_dim,
        };
        let missing = SessionId::from(format!("conformance-vector-fresh-{}", Uuid::new_v4()));
        assert!(store
            .vector_candidates_checked(&missing, &probe, &expected, 5)
            .await
            .unwrap()
            .is_empty());

        let legacy = SessionId::from(format!("conformance-vector-legacy-{}", Uuid::new_v4()));
        let interaction = NodeId::new();
        let concept = NodeId::new();
        let now = Utc::now();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&legacy, interaction, now),
                        plant_concept(
                            &legacy,
                            concept,
                            interaction,
                            "legacy unknown vector",
                            now,
                            Some(probe.clone()),
                        ),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        assert!(store
            .vector_candidates_checked(&legacy, &probe, &expected, 5)
            .await
            .unwrap()
            .is_empty());

        let contract = expected.clone();
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: legacy.clone(),
                        embedding: Some(contract.clone()),
                    }],
                },
                None,
            )
            .await
            .unwrap();
        let loaded = store.load_session(&legacy).await.unwrap();
        assert_eq!(loaded.embedding, Some(contract));
        assert!(loaded.concepts[0].embedding.is_none());
        assert!(store
            .vector_candidates_checked(&legacy, &probe, &expected, 5)
            .await
            .unwrap()
            .is_empty());

        let corrupt = SessionId::from(format!("conformance-vector-corrupt-{}", Uuid::new_v4()));
        sqlx::query("INSERT INTO sessions (session_id, embedding_dim) VALUES ($1, 1024)")
            .bind(corrupt.as_str())
            .execute(store.pool().await.unwrap())
            .await
            .unwrap();
        assert!(store
            .vector_candidates_checked(&corrupt, &probe, &expected, 5)
            .await
            .is_err());
    }

    /// P4 residual closure: `SET_ROOT_GOAL_SQL` (the UPDATE path) was
    /// compile-verified only until a live DSN was available. A `SetRootGoal`
    /// mutation through `flush` must persist the JSONB goal and read back
    /// identical; `None` clears it; the flush's session-row upsert creates the
    /// row so a bare goal write works without a prior seed.
    async fn check_set_root_goal_mutation_persists(store: &CockroachStore) {
        let sid = SessionId::from("live-set-root-goal");
        let goal = serde_json::json!({"text": "finish the demo", "n": 1});
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetRootGoal {
                        session_id: sid.clone(),
                        goal: Some(goal.clone()),
                    }],
                },
                None,
            )
            .await
            .expect("flush SetRootGoal");
        let snap = store.load_session(&sid).await.expect("load after set");
        assert_eq!(snap.root_goal, Some(goal), "goal read back identical");

        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetRootGoal {
                        session_id: sid.clone(),
                        goal: None,
                    }],
                },
                None,
            )
            .await
            .expect("flush SetRootGoal clear");
        let snap = store.load_session(&sid).await.expect("load after clear");
        assert_eq!(snap.root_goal, None, "None clears the goal");
    }

    async fn check_set_embedding_mutation_persists(store: &CockroachStore) {
        let sid = SessionId::from(format!("live-set-embedding-{}", Uuid::new_v4()));
        let embedding = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: store
                .vector_dimensions()
                .expect("Cockroach has a vector dimension"),
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(embedding.clone()),
                    }],
                },
                None,
            )
            .await
            .expect("flush SetEmbedding");
        let snap = store.load_session(&sid).await.expect("load after stamp");
        assert_eq!(snap.embedding, Some(embedding));
    }

    /// All live checks run inside ONE test/runtime — see [`new_store`] for why (pool
    /// and connections must never cross Tokio runtimes). `#[ignore]`d: without
    /// `LAMBO_COCKROACH_DSN` this must report as ignored, not skip-as-green. Each
    /// check has a distinct session namespace so re-runs against a persistent cluster
    /// are idempotent.
    #[tokio::test]
    #[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
    async fn conformance_suite() {
        let Some(dsn) = dsn_or_skip("conformance_suite") else {
            return;
        };
        let store = new_store(&dsn);
        check_init_schema_idempotent(&store).await;
        check_flush_mutations_batch_roundtrip(&store).await;
        check_load_missing_session_is_session_not_found(&store).await;
        check_vector_write_and_candidates_top1(&store).await;
        check_vector_candidates_are_session_scoped(&store).await;
        check_vector_explain_is_global_topk(&store).await;
        check_keyword_candidates_on_planted_concept(&store).await;
        check_keyword_mixed_case_ranks_like_memory_store(&store).await;
        check_legal_demote_flush_partial_index(&store).await;
        check_chunk_group_id_survives_flush_load(&store).await;
        check_embedding_contract_read_and_flush_immunity(&store).await;
        check_seed_load_full_snapshot_roundtrip(&store).await;
        check_structural_queries_agree_with_memory_store(&store).await;
        check_structural_queries_age_filter_agrees(&store).await;
        check_structural_queries_errata_derives_probe(&store).await;
        check_interaction_span_single_point_session_coverage(&store).await;
        check_record_canonization_appends_and_is_idempotent(&store).await;
        check_corrupt_contract_row_load_errors(&store).await;
        check_unstamped_vector_candidates_are_empty_until_contract_commit(&store).await;
        check_set_root_goal_mutation_persists(&store).await;
        check_set_embedding_mutation_persists(&store).await;
    }

    /// DECISION D1 item 3 camera-proof: the global vector query must execute as
    /// `vector search` on `concepts@concepts_embedding_idx` (spec §12.1 — "we used the
    /// CockroachDB distributed vector index", on camera).
    ///
    /// **T7.4 (2026-08-13) — this gate is no longer deployment-conditional.** The
    /// historical "PENDING on an index-favorable cluster" reading was wrong on both
    /// counts, and both causes are now fixed:
    ///
    /// 1. The assertion could never match its own output. The test used
    ///    `EXPLAIN (OPT, VERBOSE)`, whose operator is spelled `vector-search`
    ///    (hyphenated), and asserted the spaced `vector search` — so it failed at the
    ///    first assertion on ANY cluster, behind ANY index, even with a perfect vector
    ///    plan. See `dev-diary/evidence/20260813-131108-…-camera-proof-diagnosis.txt`.
    /// 2. `WHERE embedding IS NOT NULL` — which is load-bearing and must not be removed,
    ///    since NULL-`dist` rows hard-error the `f64` decode — cannot be proven implied
    ///    by a NON-partial vector index, so the optimizer planned a FULL SCAN. T7.4 made
    ///    `concepts_embedding_idx` itself PARTIAL on that same predicate in
    ///    `migrations/cockroach/001_init.sql`; the production query is unchanged.
    ///
    /// **Why plain `EXPLAIN` and not `EXPLAIN (OPT, VERBOSE)`** (a deliberate choice —
    /// each format spells the operator differently, and asserting the union of both
    /// spellings would be an assertion that cannot fail informatively):
    /// - Plain `EXPLAIN` is the format that literally emits `vector search`, the wording
    ///   DECISION D1 and spec §12.1 use, and it renders the proof in ~17 readable lines:
    ///   `• vector search / table: concepts@concepts_embedding_idx (partial index)`.
    /// - `OPT, VERBOSE` inlines the full 1024-element probe vector into the plan text,
    ///   producing a ~52 KB blob (measured). That is unusable as an on-camera artifact
    ///   and would make any assertion failure message unreadable — which is the entire
    ///   argument that had favoured it.
    ///
    /// The plan below was re-verified against the test's **bound** `$1`/`$2` over the
    /// extended protocol, not against literals: T7.3 round R2 established that a
    /// parameterized `LIMIT` can change plan shape, so a literal-`LIMIT` measurement
    /// would not have proven this test green.
    #[tokio::test]
    #[ignore = "camera-proof: set LAMBO_REQUIRE_VECTOR_INDEX=1 (spec §12.1 vector-index proof)"]
    async fn vector_explain_camera_proof() {
        // Kept behind its own env gate (not merged into `conformance_suite`) so the
        // §12.1 claim is asserted only when someone is deliberately capturing it, and
        // so a cluster provisioned from an older migration fails LOUDLY here rather
        // than reporting a green suite. `conformance_suite`'s
        // `check_vector_explain_is_global_topk` independently proves the DECISION D1
        // *shape* (global top-k, no session-filtered anti-pattern index) on every run.
        if env::var_os("LAMBO_REQUIRE_VECTOR_INDEX").is_none() {
            eprintln!(
                "vector_explain_camera_proof: skipped; set LAMBO_REQUIRE_VECTOR_INDEX=1 \
                 against a cluster provisioned from migrations/cockroach/001_init.sql"
            );
            return;
        }
        let Some(dsn) = dsn_or_skip("vector_explain_camera_proof") else {
            return;
        };
        let store = new_store(&dsn);
        let pool = store.pool().await.unwrap();
        let probe = encode_vector(&embed(0.5)).unwrap();
        // EXPLAIN the production statement ITSELF, not a hand-copied lookalike.
        // adve-review MINOR-4 caught the previous version claiming to be
        // "byte-for-byte" the vector-candidates query while actually dropping
        // its `::STRING` output casts. The casts cannot change index selection,
        // so the finding was cosmetic — but the entire value of this proof is
        // that it explains the query production runs, so the claim has to be
        // true by construction rather than by careful copying. B0 keeps that
        // property by reading the statement off **this store's own**
        // `DialectSql` (`store.sql`), the exact string `vector_candidates`
        // issues, so neither an edit to the SQL nor a change of dialect can
        // silently desynchronize the camera proof from it.
        let vector_candidates_sql = &store.sql.vector_candidates;
        let rows = sqlx::query(&format!("EXPLAIN {vector_candidates_sql}"))
            .bind(&probe)
            .bind(5i64)
            .fetch_all(pool)
            .await
            .map_err(backend)
            .unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, usize>(0).map_err(backend))
            .collect::<Result<_, _>>()
            .unwrap();
        let text = plan.join("\n");
        // The plan IS the artifact — print it so a `--nocapture` run is the camera shot.
        eprintln!("vector_explain_camera_proof plan:\n{text}");
        assert!(
            text.contains("vector search"),
            "EXPLAIN must show `vector search` (plain-EXPLAIN spelling), got:\n{text}"
        );
        assert!(
            text.contains("concepts@concepts_embedding_idx"),
            "EXPLAIN must show the vector search on concepts@concepts_embedding_idx, got:\n{text}"
        );
        // The exact regression T7.4 fixed: with a NON-partial index the predicate forces
        // `spans: FULL SCAN` on concepts_pkey. Assert it is gone, so a cluster that
        // silently reverts to a non-partial index fails with a pointed message.
        assert!(
            !text.contains("FULL SCAN"),
            "EXPLAIN must not fall back to a full scan (non-partial index?), got:\n{text}"
        );
    }
}

/// H2 — the **live Cockroach leg** of cross-store recall parity
/// (`dev-diary/lambo-for-mooshik/H-cross-store-parity.md`, §H2): the H1
/// harness's corpus, probe × limit grid, and v1 report shape, run against a
/// real cluster through [`CockroachStore`] with `LAMBO_COCKROACH_DSN`.
///
/// **Design note — twin, not shared extraction.** The report types, the three
/// agreement measures, `probe_set` and the memory oracle are deliberately
/// re-instantiated here rather than extracted into a shared module both test
/// files consume. That follows this repo's recorded precedent for exactly
/// this situation — F's `cosine_oracle` and H1's own `MemoryOracleStore` are
/// each "reimplemented rather than reused because the original is private to
/// another adapter's test module" — and it keeps H1's landed, evidence-
/// documented module (its paths are cited verbatim in
/// `evidence/mooshik-h1-cross-store-parity/README.md`) untouched. Drift
/// between the twins is bounded by what actually matters for comparability:
/// the *serde shape* of `ParityReport`, which is schema-versioned and whose
/// stability contract ("add rows, never fields") is enforced by review, not
/// by a shared type.
///
/// What is asserted live (the §H2 attribution rule):
/// * **Score skew zero on the shared scale.** Every pair's scores arrive
///   already converted to `1 − d²/2 ≡ cosine` inside each adapter's own
///   `vector_candidates_checked`. Exact-scan pairs must be bit-for-bit equal;
///   ANN-vs-exact pairs may differ only by float32 round-trip noise
///   ([`SCORE_SKEW_EPSILON`]) — any systematic conversion skew (the H3-named
///   danger: `1 − d` vs `1 − d²/2`) shows up orders of magnitude above that
///   bound and fails here.
/// * **ANN divergence within the envelope.** C-SPANN's published figure is
///   0.99 recall@50 at beam 64; at equal-size top-k sets,
///   `jaccard ≥ 0.98 ⇔ recall ≥ 0.99`, so every cockroach-vs-exact pair is
///   asserted against [`ANN_JACCARD_FLOOR`] while the actual divergence
///   (jaccard, rank prefix, displacement) is carried in the report.
/// * **Quarantine-history equivalence** (the question F could only reason
///   about): the same write history — stamp contract A, write vectors,
///   restamp contract B at a different width — must yield the same
///   *observable* recall behaviour on Cockroach (NULL-only quarantine + DDL
///   width enforcement) and SQLite (write-gate + restamp-quarantine): zero
///   answers delivered out of the abandoned embedding space, either as an
///   empty result or as a fail-closed refusal. The mechanisms differ by
///   design; the leg measures whether the recall behaviour does.
#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]
mod h2_cockroach_parity {
    use super::*;
    use crate::fixtures::load_snapshot;
    use crate::MemoryStore;
    use serde::{Deserialize, Serialize};
    use std::collections::{HashMap, HashSet};
    use std::env;

    // ---- Report schema (v1 — twin of H1's; see that module's doc) --------

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ParityReport {
        schema_version: u32,
        harness: HarnessInfo,
        adapters: Vec<AdapterRun>,
        pairs: Vec<PairResult>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct HarnessInfo {
        /// Populated by whatever captures this report, never by the harness
        /// itself. `null` here, like in H1's committed report; the capture
        /// rev lives in the evidence README instead.
        git_rev: Option<String>,
        features: Vec<String>,
        fixtures: Vec<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum ScanKind {
        Exact,
        Ann,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum Attribution {
        ExactMustMatch,
        AnnEnvelope,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct AdapterRun {
        name: String,
        scan: ScanKind,
        index_present: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct IdDisplacement {
        id: String,
        rank_a: usize,
        rank_b: usize,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct PairResult {
        fixture: String,
        probe: String,
        limit: usize,
        adapter_a: String,
        adapter_b: String,
        attribution: Attribution,
        candidate_jaccard: f64,
        rank_prefix_match: usize,
        displacement: Vec<IdDisplacement>,
        max_score_diff: f64,
        exact_match: bool,
    }

    // ---- Live-run constants ---------------------------------------------

    /// The cluster column width: `concepts.embedding VECTOR(1024)` in
    /// `migrations/cockroach/001_init.sql`. The corpus is the SAME SHAPE as
    /// H1's synthetic leg (both fixture graphs, `synthetic_unit_vector`, the
    /// same four probe shapes × five limits) at the width the live DDL
    /// admits — width is a property of the store, not of the corpus design.
    const DIM: usize = 1024;

    /// Float32 round-trip noise bound for score agreement on the shared
    /// `1 − d²/2 ≡ cosine` scale: the oracle computes f32 cosine directly;
    /// Cockroach accumulates an f32 L2 over 1024 dims and converts with
    /// `distance_to_score`. Both err by a few ulps (~1e-6 worst case); any
    /// conversion skew worth catching (`1 − d`, a scale factor, …) lands 3+
    /// orders of magnitude above this line. The measured maximum goes into
    /// the report and evidence README regardless.
    const SCORE_SKEW_EPSILON: f64 = 1e-4;

    /// C-SPANN published 0.99 recall@50 at beam 64. At equal-size top-k sets
    /// jaccard = inter/(2k − inter) and recall = inter/k, so jaccard ≥ 0.98
    /// ⟺ recall ≥ 0.99: the published envelope expressed in the measure the
    /// report already carries.
    const ANN_JACCARD_FLOOR: f64 = 0.98;

    fn synthetic_unit_vector(i: usize, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim)
            .map(|j| {
                // Same construction as H1's helper: integer math, one divide,
                // deterministic across runs and platforms.
                let k = ((i + 1) * 37 + (j + 1) * 11) % 97;
                (k as f32) / 97.0 - 0.5
            })
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm > 1e-6, "degenerate synthetic vector for i={i}");
        for x in &mut v {
            *x /= norm;
        }
        v
    }

    fn h2_contract(dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: Some("h2-live-model".into()),
            dim,
        }
    }

    // ---- The memory oracle twin (same precedent as H1's) ----------------

    /// `MemoryStore` plus an exact-cosine vector leg — reimplemented rather
    /// than reused from H1's private module, per the precedent its doc
    /// comment records (which itself cites F's `cosine_oracle`).
    struct MemoryOracleStore {
        inner: MemoryStore,
    }

    impl MemoryOracleStore {
        fn new() -> Self {
            Self {
                inner: MemoryStore::new(),
            }
        }
    }

    #[async_trait]
    impl GraphStore for MemoryOracleStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities() | Capabilities::VECTOR_SEARCH
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.inner.flush(batch, token).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            panic!("MemoryOracleStore: unchecked vector lookup is unused by the H2 harness")
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            validate_vector_candidate_limit(limit)?;
            let snapshot = match self.inner.load_session(session).await {
                Ok(s) => s,
                Err(StoreError::SessionNotFound(_)) => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            match &snapshot.embedding {
                None => return Ok(Vec::new()),
                Some(durable) if durable == expected_contract => {}
                Some(durable) => {
                    return Err(StoreError::Invariant(format!(
                        "vector candidate lookup refused after embedding contract changed: \
                         vectors were written by kind={} model={:?} dim={}, but the caller \
                         expects kind={} model={:?} dim={}",
                        durable.kind,
                        durable.model,
                        durable.dim,
                        expected_contract.kind,
                        expected_contract.model,
                        expected_contract.dim,
                    )));
                }
            }
            let mut scored: Vec<Scored<NodeId>> = snapshot
                .concepts
                .iter()
                .filter_map(|c| {
                    let vector = c.embedding.as_ref()?;
                    Some(Scored::new(
                        c.id,
                        f64::from(crate::embed::cosine(embedding, vector)),
                    ))
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| a.item.0.cmp(&b.item.0))
            });
            scored.truncate(limit);
            Ok(scored)
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
                .blast_radius(session, node, min_edge_age, now)
                .await
        }
        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    // ---- The three agreement measures (twins of H1's) -------------------

    fn jaccard(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> f64 {
        let sa: HashSet<NodeId> = a.iter().map(|s| s.item).collect();
        let sb: HashSet<NodeId> = b.iter().map(|s| s.item).collect();
        if sa.is_empty() && sb.is_empty() {
            return 1.0;
        }
        let inter = sa.intersection(&sb).count() as f64;
        let union = sa.union(&sb).count() as f64;
        inter / union
    }

    fn rank_prefix_match(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> usize {
        a.iter()
            .zip(b.iter())
            .take_while(|(x, y)| x.item == y.item)
            .count()
    }

    fn displacements(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> Vec<IdDisplacement> {
        let rank_a: HashMap<NodeId, usize> =
            a.iter().enumerate().map(|(i, s)| (s.item, i)).collect();
        let rank_b: HashMap<NodeId, usize> =
            b.iter().enumerate().map(|(i, s)| (s.item, i)).collect();
        let mut out: Vec<IdDisplacement> = rank_a
            .iter()
            .filter_map(|(id, &ra)| {
                rank_b.get(id).and_then(|&rb| {
                    (ra != rb).then_some(IdDisplacement {
                        id: id.0.to_string(),
                        rank_a: ra,
                        rank_b: rb,
                    })
                })
            })
            .collect();
        out.sort_by_key(|d| d.rank_a);
        out
    }

    fn max_score_diff(a: &[Scored<NodeId>], b: &[Scored<NodeId>]) -> f64 {
        let scores_b: HashMap<NodeId, f64> = b.iter().map(|s| (s.item, s.score)).collect();
        a.iter()
            .filter_map(|s| scores_b.get(&s.item).map(|&sb| (s.score - sb).abs()))
            .fold(0.0_f64, f64::max)
    }

    // ---- Probe/limit grid (same shapes as H1/F) --------------------------

    fn probe_set(pool: &[(NodeId, Vec<f32>)]) -> Vec<(&'static str, Vec<f32>)> {
        let first = &pool[0].1;
        let second = &pool[1].1;
        let negated: Vec<f32> = first.iter().map(|x| -x).collect();
        let midpoint: Vec<f32> = {
            let mut m: Vec<f32> = first
                .iter()
                .zip(second.iter())
                .map(|(a, b)| (a + b) / 2.0)
                .collect();
            let n = m.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in &mut m {
                *x /= n;
            }
            m
        };
        let off_axis = synthetic_unit_vector(usize::from(u8::MAX), DIM);
        vec![
            ("stored-itself", first.clone()),
            ("negated", negated),
            ("midpoint", midpoint),
            ("off-axis", off_axis),
        ]
    }

    struct Adapter {
        name: &'static str,
        scan: ScanKind,
        index_present: bool,
        store: Box<dyn GraphStore>,
    }

    // ---- DSN gating (same convention as the `conformance` module) --------

    fn dsn() -> Option<String> {
        env::var("LAMBO_COCKROACH_DSN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Same contract as `conformance::dsn_or_skip`: without a DSN this prints
    /// a skip notice (surfacing as ignored, never skip-as-green), and under
    /// `LAMBO_REQUIRE_LIVE=1` a missing DSN panics. The DSN is read from the
    /// environment only and never printed.
    fn dsn_or_skip(test: &str) -> Option<String> {
        match dsn() {
            Some(d) => Some(d),
            None => {
                if env::var_os("LAMBO_REQUIRE_LIVE").is_some() {
                    panic!(
                        "{test}: LAMBO_COCKROACH_DSN is unset but LAMBO_REQUIRE_LIVE is set — \
                         refusing to skip a live cockroach test"
                    );
                }
                eprintln!("SKIP {test}: LAMBO_COCKROACH_DSN not set");
                None
            }
        }
    }

    fn cfg(dsn: String) -> StoreConfig {
        StoreConfig {
            kind: crate::store::StoreKind::Cockroach,
            dsn: Some(dsn),
            path: None,
            vector_dim: None,
        }
    }

    /// One pool per run, used on ONE test runtime — see `conformance::new_store`.
    fn new_store(dsn: &str) -> CockroachStore {
        CockroachStore::new(cfg(dsn.to_string())).unwrap()
    }

    // ---- Cluster-shape + index capture (read-only) -----------------------

    /// Camera-proofs that the production query plans as `vector search` on
    /// `concepts@concepts_embedding_idx` (DECISION D1, spec §12.1), and
    /// RETURNS whether it does, so the caller can stamp the report's
    /// `index_present` for the cockroach adapter from the measurement itself
    /// rather than a hardcoded literal (H2-R1-2). The plan is printed so the
    /// `--nocapture` log carries it.
    async fn assert_index_backed(store: &CockroachStore) -> bool {
        let pool = store.pool().await.unwrap();
        let probe = encode_vector(&synthetic_unit_vector(usize::from(u8::MAX), DIM)).unwrap();
        // The store's own composed statement, for the reason
        // `vector_explain_camera_proof` spells out: the plan has to be the plan
        // of the query production runs.
        let vector_candidates_sql = &store.sql.vector_candidates;
        let rows = sqlx::query(&format!("EXPLAIN {vector_candidates_sql}"))
            .bind(&probe)
            .bind(5i64)
            .fetch_all(pool)
            .await
            .unwrap();
        let text: String = rows
            .iter()
            .map(|r| r.try_get::<String, usize>(0).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("H2 EXPLAIN plan:\n{text}");
        let vector_search = text.contains("vector search");
        let index_hit = text.contains("concepts@concepts_embedding_idx");
        let no_full_scan = !text.contains("FULL SCAN");
        assert!(
            vector_search,
            "H2: EXPLAIN must show `vector search`, got:\n{text}"
        );
        assert!(
            index_hit,
            "H2: EXPLAIN must show concepts@concepts_embedding_idx, got:\n{text}"
        );
        assert!(
            no_full_scan,
            "H2: EXPLAIN must not fall back to a full scan, got:\n{text}"
        );
        vector_search && index_hit && no_full_scan
    }

    /// Read-only cluster shape for the evidence log: version string and the
    /// full `SHOW CREATE TABLE concepts` (which carries the vector index DDL
    /// with its parameters). Never prints connection material.
    async fn log_cluster_shape(store: &CockroachStore) {
        let pool = store.pool().await.unwrap();
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(pool)
            .await
            .unwrap();
        eprintln!("H2 cluster version(): {version}");
        let beam_env = match env::var(VECTOR_BEAM_SIZE_ENV) {
            Ok(v) => format!("={v}"),
            Err(_) => "unset".to_string(),
        };
        eprintln!(
            "H2 beam size: default {DEFAULT_VECTOR_BEAM_SIZE} (LAMBO_VECTOR_BEAM_SIZE {beam_env})"
        );
        let ddl: Vec<String> = sqlx::query("SHOW CREATE TABLE concepts")
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .filter_map(|r| r.try_get::<Option<String>, usize>(1).ok().flatten())
            .collect();
        eprintln!("H2 SHOW CREATE TABLE concepts:\n{}\n", ddl.join("\n"));
    }

    // ---- Corpus seeding --------------------------------------------------

    /// Re-home a fixture snapshot into a session scope this run owns: fresh
    /// node ids (mapped consistently across interactions, origins and edges)
    /// under a unique per-run session id, so re-runs and concurrent runs on a
    /// persistent shared cluster can never cross-contaminate — the same
    /// convention the conformance suite's unique-session-per-run follows.
    fn rebase_into_fresh_scope(snap: &mut GraphSnapshot, suffix: &str) {
        let sid = SessionId::from(format!("{}-h2-{}", snap.session_id.as_str(), suffix));
        let mut ids: HashMap<NodeId, NodeId> = HashMap::new();
        for i in &snap.interactions {
            ids.insert(i.id, NodeId::new());
        }
        for c in &snap.concepts {
            ids.insert(c.id, NodeId::new());
        }
        for i in &mut snap.interactions {
            i.id = ids[&i.id];
            i.session_id = sid.clone();
            if let Some(p) = i.previous_id {
                i.previous_id = Some(ids[&p]);
            }
        }
        for c in &mut snap.concepts {
            c.id = ids[&c.id];
            c.session_id = sid.clone();
            c.origin_interaction = ids[&c.origin_interaction];
        }
        for e in &mut snap.edges {
            e.id = NodeId::new();
            e.session_id = sid.clone();
            e.source = ids[&e.source];
            e.target = ids[&e.target];
        }
        snap.session_id = sid;
    }

    /// Structure first (interactions are FK targets), then the contract stamp,
    /// then the vector-bearing concepts — the write-gate ordering F1 relies on.
    fn seed_batch(snap: &GraphSnapshot, contract: &EmbeddingContract) -> MutationBatch {
        let mut mutations: Vec<Mutation> = snap
            .interactions
            .iter()
            .map(|i| Mutation::UpsertNode {
                node: Node::Interaction(i.clone()),
            })
            .collect();
        mutations.push(Mutation::SetEmbedding {
            session_id: snap.session_id.clone(),
            embedding: Some(contract.clone()),
        });
        for c in &snap.concepts {
            mutations.push(Mutation::UpsertNode {
                node: Node::Concept(c.clone()),
            });
        }
        for e in &snap.edges {
            mutations.push(Mutation::UpsertEdge { edge: e.clone() });
        }
        MutationBatch { mutations }
    }

    fn active_features() -> Vec<String> {
        let mut v = vec!["fixtures".to_string()];
        for (flag, name) in [
            (cfg!(feature = "store-sqlite"), "store-sqlite"),
            (cfg!(feature = "store-memory"), "store-memory"),
            (cfg!(feature = "store-cockroach"), "store-cockroach"),
            (cfg!(feature = "embed-fixture"), "embed-fixture"),
        ] {
            if flag {
                v.push(name.to_string());
            }
        }
        v
    }

    // ---- The grid --------------------------------------------------------

    /// One fixture rebased into the run's scope, seeded into every adapter,
    /// then the identical probe × limit grid H1 runs, pairwise across all
    /// adapters. Exact-scan pairs are asserted bit-for-bit equal; every pair
    /// touching the ANN adapter is asserted against the skew and jaccard
    /// bounds above AND carried in the report with its actual numbers.
    async fn run_fixture_grid(
        fixture_label: &str,
        snap: &GraphSnapshot,
        contract: &EmbeddingContract,
        adapters: &mut [Adapter],
        report: &mut ParityReport,
    ) {
        let sid = snap.session_id.clone();
        let batch = seed_batch(snap, contract);
        // Idempotent on every adapter (`IF NOT EXISTS` DDL / in-memory
        // constructors); required here because the sqlite leg's in-memory
        // database starts empty.
        for a in adapters.iter() {
            a.store.init_schema().await.unwrap();
        }
        for a in adapters.iter() {
            a.store.flush(&batch, None).await.unwrap();
        }
        for a in adapters.iter() {
            if !report.adapters.iter().any(|r| r.name == a.name) {
                report.adapters.push(AdapterRun {
                    name: a.name.to_string(),
                    scan: a.scan,
                    index_present: a.index_present,
                });
            }
        }
        if !report.harness.fixtures.iter().any(|f| f == fixture_label) {
            report.harness.fixtures.push(fixture_label.to_string());
        }

        let pool: Vec<(NodeId, Vec<f32>)> = snap
            .concepts
            .iter()
            .map(|c| {
                (
                    c.id,
                    c.embedding.clone().expect("harness vectored every concept"),
                )
            })
            .collect();
        let probes = probe_set(&pool);
        let limits = [1usize, 3, 5, pool.len(), pool.len() + 7];

        for i in 0..adapters.len() {
            for j in (i + 1)..adapters.len() {
                let (a_name, a_scan, a_store) =
                    (&adapters[i].name, &adapters[i].scan, &adapters[i].store);
                let (b_name, b_scan, b_store) =
                    (&adapters[j].name, &adapters[j].scan, &adapters[j].store);
                let attribution = if *a_scan == ScanKind::Exact && *b_scan == ScanKind::Exact {
                    Attribution::ExactMustMatch
                } else {
                    Attribution::AnnEnvelope
                };
                for (probe_label, probe) in &probes {
                    for &limit in &limits {
                        let got_a = a_store
                            .vector_candidates_checked(&sid, probe, contract, limit)
                            .await
                            .unwrap();
                        let got_b = b_store
                            .vector_candidates_checked(&sid, probe, contract, limit)
                            .await
                            .unwrap();
                        // H2-R1-1 hardening, schema-v1-discipline choice:
                        // `PairResult` is pinned by report schema v1 (twin of
                        // H1's committed evidence), so candidate counts are
                        // NOT added as fields; instead the harness asserts
                        // non-emptiness inline. Without this, jaccard()'s
                        // both-empty → 1.0 special case would let an all-empty
                        // regression pass vacuously with a perfect-looking
                        // report. The quarantine leg (which legitimately
                        // expects empty answers) runs separately and is
                        // intentionally not covered here.
                        assert!(
                            !got_a.is_empty() && !got_b.is_empty(),
                            "H2: empty candidate set from {a_name} or {b_name} on \
                             fixture {fixture_label:?} probe {probe_label:?} limit \
                             {limit} — refusing to score a possibly-vacuous cell"
                        );
                        let pair = PairResult {
                            fixture: fixture_label.to_string(),
                            probe: (*probe_label).to_string(),
                            limit,
                            adapter_a: (*a_name).to_string(),
                            adapter_b: (*b_name).to_string(),
                            attribution,
                            candidate_jaccard: jaccard(&got_a, &got_b),
                            rank_prefix_match: rank_prefix_match(&got_a, &got_b),
                            displacement: displacements(&got_a, &got_b),
                            max_score_diff: max_score_diff(&got_a, &got_b),
                            exact_match: got_a == got_b,
                        };
                        if attribution == Attribution::ExactMustMatch {
                            assert!(
                                pair.exact_match,
                                "H2: exact-scan adapters {a_name} and {b_name} disagree on \
                                 fixture {fixture_label:?} probe {probe_label:?} limit {limit}: \
                                 jaccard={} prefix={} score_diff={} displacement={:?}",
                                pair.candidate_jaccard,
                                pair.rank_prefix_match,
                                pair.max_score_diff,
                                pair.displacement,
                            );
                        } else {
                            assert!(
                                pair.candidate_jaccard >= ANN_JACCARD_FLOOR,
                                "H2: ANN candidate divergence beyond the C-SPANN envelope on \
                                 {fixture_label:?} probe {probe_label:?} limit {limit} \
                                 ({a_name} vs {b_name}): jaccard {} < {ANN_JACCARD_FLOOR}",
                                pair.candidate_jaccard
                            );
                            assert!(
                                pair.max_score_diff <= SCORE_SKEW_EPSILON,
                                "H2: systematic score skew on the shared 1 − d²/2 scale \
                                 ({a_name} vs {b_name}, {fixture_label:?} probe {probe_label:?} \
                                 limit {limit}): max diff {} > {SCORE_SKEW_EPSILON}",
                                pair.max_score_diff
                            );
                        }
                        report.pairs.push(pair);
                    }
                }
            }
        }
    }

    // ---- The quarantine-history leg --------------------------------------

    /// F's reasoned-but-unmeasured question, settled with a measurement: the
    /// SAME history of writes — stamp contract A (width 1024), write vectors,
    /// restamp contract B (width 4, different model) — must produce the same
    /// OBSERVABLE recall behaviour on Cockroach and SQLite: zero answers out
    /// of the abandoned space. The mechanisms differ by design (Cockroach:
    /// NULL-only quarantine + `VECTOR(1024)` DDL width enforcement; SQLite:
    /// width-change restamp-quarantine + its own width gate); this leg
    /// measures whether the recall behaviour does.
    ///
    /// Needs SQLite compiled in for the comparison side; skips cleanly (with
    /// a message) when it isn't.
    async fn quarantine_restamp_leg(adapters: &[Adapter], suffix: &str) {
        let cr = adapters
            .iter()
            .find(|a| a.name == "cockroach")
            .expect("cockroach adapter always present");
        let Some(sq) = adapters.iter().find(|a| a.name == "sqlite") else {
            eprintln!(
                "H2 quarantine leg skipped: sqlite adapter not compiled in (build with \
                 --features store-sqlite to compare quarantine behaviour)"
            );
            return;
        };

        let mut snap = load_snapshot("session-drift").unwrap();
        rebase_into_fresh_scope(&mut snap, &format!("{suffix}-q"));
        let contract_a = h2_contract(DIM);
        for (i, c) in snap.concepts.iter_mut().enumerate() {
            c.embedding = Some(synthetic_unit_vector(i, DIM));
        }
        let batch = seed_batch(&snap, &contract_a);
        cr.store.init_schema().await.unwrap();
        sq.store.init_schema().await.unwrap();
        cr.store.flush(&batch, None).await.unwrap();
        sq.store.flush(&batch, None).await.unwrap();

        // Recall under A works on both before the restamp.
        let probe_a = synthetic_unit_vector(0, DIM);
        let cr_before = cr
            .store
            .vector_candidates_checked(&snap.session_id, &probe_a, &contract_a, 5)
            .await
            .unwrap();
        let sq_before = sq
            .store
            .vector_candidates_checked(&snap.session_id, &probe_a, &contract_a, 5)
            .await
            .unwrap();

        // Restamp to a DIFFERENT width — the history that makes the two
        // quarantine designs diverge mechanically.
        let contract_b = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("h2-restamp-model".into()),
            dim: 4,
        };
        let restamp = MutationBatch {
            mutations: vec![Mutation::SetEmbedding {
                session_id: snap.session_id.clone(),
                embedding: Some(contract_b.clone()),
            }],
        };
        cr.store.flush(&restamp, None).await.unwrap();
        sq.store.flush(&restamp, None).await.unwrap();

        // SQLite: the restamp-quarantine physically NULLed every concept
        // vector (observable via load_session), so the width-4 recall
        // question is VALID under the new stamp but answers EMPTY — the
        // old space is gone, not merely hidden.
        let sq_snap = sq.store.load_session(&snap.session_id).await.unwrap();
        let quarantined = sq_snap
            .concepts
            .iter()
            .filter(|c| c.embedding.is_none())
            .count();
        assert_eq!(
            quarantined,
            sq_snap.concepts.len(),
            "H2: sqlite restamp-quarantine must NULL every concept vector"
        );
        assert!(
            !sq_snap.concepts.is_empty(),
            "H2: quarantine leg seeded no concepts"
        );
        let probe_b = synthetic_unit_vector(1, 4);
        let sq_after = sq
            .store
            .vector_candidates_checked(&snap.session_id, &probe_b, &contract_b, 5)
            .await;
        let sq_after = sq_after.unwrap();
        assert!(
            sq_after.is_empty(),
            "H2: sqlite must answer empty (not stale vectors) after restamp-quarantine, \
             got {} candidates",
            sq_after.len()
        );

        // Cockroach: no physical restamp-quarantine fires (its NULL-only
        // quarantine targets the unstamped→stamped transition), but the
        // VECTOR(1024) DDL width enforcement refuses the width-4 question
        // outright — again, nothing from space A can answer.
        let cr_after = cr
            .store
            .vector_candidates_checked(&snap.session_id, &probe_b, &contract_b, 5)
            .await;
        assert!(
            cr_after.is_err(),
            "H2: cockroach must refuse a width-4 recall question (DDL width {DIM}), got {:?}",
            cr_after
        );

        eprintln!(
            "H2 quarantine leg: under contract A both answered non-empty (cockroach {}, \
             sqlite {}); after restamping B(dim 4): sqlite quarantined {quarantined}/{} \
             concept vectors to NULL and answered EMPTY; cockroach refused via DDL width \
             enforcement. Cross-space recall delivered: 0 candidates on BOTH adapters.",
            cr_before.len(),
            sq_before.len(),
            sq_snap.concepts.len(),
        );
    }

    /// **Acceptance: H2's Done-when box** — "live Cockroach run, skew zero on
    /// the score scale, ANN divergence stated with numbers against the
    /// C-SPANN envelope".
    ///
    /// `#[ignore]`d like every live cockroach test. Run:
    /// `LAMBO_REQUIRE_LIVE=1 cargo test --features store-cockroach,fixtures \
    ///  --lib store::pg::cockroach::h2_cockroach_parity -- --ignored --nocapture`
    /// Set `LAMBO_H2_EMIT_EVIDENCE=1` to also write the JSON report to
    /// `evidence/mooshik-h2-cockroach-parity/report.json`.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn h2_live_cockroach_recall_parity() {
        let Some(dsn) = dsn_or_skip("h2_live_cockroach_recall_parity") else {
            return;
        };
        let suffix = Uuid::new_v4().simple().to_string();

        let mut report = ParityReport {
            schema_version: 1,
            harness: HarnessInfo {
                git_rev: None,
                features: active_features(),
                fixtures: Vec::new(),
            },
            adapters: Vec::new(),
            pairs: Vec::new(),
        };

        let mut adapters: Vec<Adapter> = Vec::new();
        let cr = new_store(&dsn);
        cr.init_schema().await.unwrap();
        log_cluster_shape(&cr).await;
        let index_present = assert_index_backed(&cr).await;
        adapters.push(Adapter {
            name: "cockroach",
            scan: ScanKind::Ann,
            index_present,
            store: Box::new(cr),
        });
        adapters.push(Adapter {
            name: "memory-oracle",
            scan: ScanKind::Exact,
            index_present: false,
            store: Box::new(MemoryOracleStore::new()) as Box<dyn GraphStore>,
        });
        #[cfg(feature = "store-sqlite")]
        adapters.push(Adapter {
            name: "sqlite",
            scan: ScanKind::Exact,
            index_present: false,
            store: Box::new(
                crate::store::SqliteStore::connect("sqlite::memory:")
                    .unwrap()
                    .with_vector_dim(DIM)
                    .unwrap(),
            ) as Box<dyn GraphStore>,
        });

        // Pinned exactly: 2 fixtures × 4 probes × 5 limits × C(n, 2) pairs.
        let n_pairs_per_cell = adapters.len() * (adapters.len() - 1) / 2;

        let contract = h2_contract(DIM);

        for fixture in ["session-rest-api", "session-drift"] {
            let mut snap: GraphSnapshot = load_snapshot(fixture).unwrap();
            for (i, c) in snap.concepts.iter_mut().enumerate() {
                c.embedding = Some(synthetic_unit_vector(i, DIM));
            }
            run_fixture_grid(fixture, &snap, &contract, &mut adapters, &mut report).await;
        }

        assert_eq!(
            report.pairs.len(),
            2 * 4 * 5 * n_pairs_per_cell,
            "H2: grid matrix dimensions drifted"
        );

        // Harness honesty check (mirrors H1's): every recorded ExactMustMatch
        // row was asserted, not just reported.
        assert!(
            report
                .pairs
                .iter()
                .all(|p| p.attribution != Attribution::ExactMustMatch || p.exact_match),
            "H2: an ExactMustMatch pair was recorded without being asserted — harness bug"
        );

        quarantine_restamp_leg(&adapters, &suffix).await;

        // Summary numbers for the run log and the evidence README.
        let ann_pairs: Vec<&PairResult> = report
            .pairs
            .iter()
            .filter(|p| p.attribution == Attribution::AnnEnvelope)
            .collect();
        let min_jaccard = ann_pairs
            .iter()
            .map(|p| p.candidate_jaccard)
            .fold(f64::INFINITY, f64::min);
        let max_skew = report
            .pairs
            .iter()
            .map(|p| p.max_score_diff)
            .fold(0.0_f64, f64::max);
        let total_displacement: usize = report.pairs.iter().map(|p| p.displacement.len()).sum();
        let fully_agreeing_ann_cells = ann_pairs
            .iter()
            .filter(|p| p.displacement.is_empty() && p.candidate_jaccard == 1.0)
            .count();
        println!(
            "H2: {} pairs across fixtures ({}) — adapters {:?}; ANN pairs: min jaccard \
             {min_jaccard}, max score skew {max_skew}, total displaced ranks \
             {total_displacement}; fully-agreeing ANN cells: {fully_agreeing_ann_cells}/{}",
            report.pairs.len(),
            report.harness.fixtures.join(", "),
            report
                .adapters
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ann_pairs.len(),
        );

        if env::var_os("LAMBO_H2_EMIT_EVIDENCE").is_some() {
            let dir = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/evidence/mooshik-h2-cockroach-parity"
            );
            std::fs::create_dir_all(dir).unwrap();
            let json = serde_json::to_string_pretty(&report).unwrap();
            std::fs::write(format!("{dir}/report.json"), json).unwrap();
        }
    }
}
