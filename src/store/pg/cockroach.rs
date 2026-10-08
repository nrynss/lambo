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
mod tests;

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
mod conformance;

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
mod h2_cockroach_parity;
