//! The `Dialect` trait: everything [`super::PgStore`] cannot say the same way
//! on every Postgres-wire-protocol engine.
//!
//! **Compile-time, monomorphized, no dynamic dispatch.** `PgStore<D: Dialect>`
//! takes the dialect as a type parameter and every method below is an
//! associated const or an associated function, so a dialect call costs nothing
//! at runtime and a missing dialect is a compile error rather than a panic.
//!
//! **The surface starts as the §B3 table** in
//! `dev-diary/lambo-for-mooshik/B-postgres-store.md`: the DDL, two casts, the
//! distance operator and its score conversion, and the width authority. B0
//! shipped one working dialect. B1 adds `PostgresDialect` as a named,
//! fail-closed stub (no Cockroach SQL). B2 fills its DDL and splits the
//! over-merged `init_schema` / `connect_options` rows (Cockroach-only
//! `endpoint STRING` DDL and `vector_search_beam_size`). Those extra rows
//! were discovered by diffing two real implementations, not guessed from
//! one. B3 fills the ranking conversion (`distance_to_score`): Cockroach
//! keeps L2 `1 - d^2/2`, Postgres is cosine distance `1 - d`.
//!
//! **The over-merging trap, restated where it bites.** A statement belongs in
//! [`super::PgStore`] only when its SQL is byte-identical for every dialect.
//! A statement that differs by one cast is composed from the consts below, and
//! a statement that differs by more than that does not belong in the shared
//! base at all. There is no `bool is_cockroach` and there is no `if cockroach`:
//! a base full of engine branches recreates the drift problem inside the shared
//! code, where it is harder to see.

use std::borrow::Cow;

use crate::store::{StoreConfig, StoreError};

/// One Postgres-wire-protocol engine's spelling of the handful of things
/// [`super::PgStore`] cannot write once.
///
/// `Send + Sync + 'static` is a property of the *marker type*, not of any
/// behaviour: `PgStore<D>` is handed out as a `Box<dyn GraphStore>` and must
/// stay `Send + Sync`, and `D` appears in its `PhantomData`.
pub trait Dialect: Send + Sync + 'static {
    /// The full schema this dialect provisions, at a dense-vector width of
    /// `dim`.
    ///
    /// Returns `Cow` because the two authorities are genuinely different
    /// shapes: a dialect whose schema file is the contract hands back a
    /// borrowed `include_str!`, while a dialect that substitutes a configured
    /// width into its DDL hands back an owned `String`. `dim` is the width
    /// [`Dialect::vector_dim`] already resolved, so a static-DDL dialect
    /// asserts against it rather than ignoring it.
    fn init_sql(dim: usize) -> Result<Cow<'static, str>, StoreError>;

    /// This dialect's cast to the text type, applied to columns whose value
    /// travels to Rust as a `String` (ids, vectors, JSONB documents).
    const STRING_CAST: &'static str;

    /// This dialect's cast to the dense-vector type, applied to a placeholder
    /// whose value travels from Rust as a text literal.
    const VECTOR_CAST: &'static str;

    /// The nearest-neighbour operator the recall query orders by. Ascending
    /// order is "most similar first" for every operator we accept here.
    const DISTANCE_OP: &'static str;

    /// Convert one [`Dialect::DISTANCE_OP`] result into the similarity score
    /// `GraphStore::vector_candidates` promises, on the scale
    /// `semantic_match_threshold` is written against.
    ///
    /// **This is the dangerous one.** Getting it wrong does not fail: it ranks
    /// wrongly, quietly, and looks like a model quality problem. Every
    /// implementation carries the reasoning that makes its formula equal
    /// cosine similarity, not just the formula.
    fn distance_to_score(dist: f64) -> f64;

    /// The store-authoritative dense-vector width (spec §3.3, "vector width is
    /// not a global constant"), taken from whatever this dialect treats as the
    /// authority: a parsed DDL, or config.
    ///
    /// `cfg` is offered rather than assumed: a dialect whose schema file
    /// carries the width ignores it, and says so in its own doc.
    fn vector_dim(cfg: &StoreConfig) -> Result<usize, StoreError>;

    /// Optional live-database probe of the dense-vector column width.
    ///
    /// Default: none. Cockroach's authority is the static `001_init.sql` file
    /// parsed at construction; a live read would echo that file. Postgres
    /// substitutes width *into* the template, so the initialized
    /// `vector(n)` must be read from the database (B4). The statement, if
    /// present, returns one text column in `format_type` form
    /// (`vector(768)`).
    fn live_schema_vector_width_sql() -> Option<&'static str> {
        None
    }

    /// Operator-facing dialect name in preflight errors (`"cockroach"` /
    /// `"postgres"`). Discovered by splitting B0-N1: the shared
    /// `preflight_schema` had hard-coded `"cockroach"`.
    const NAME: &'static str;

    /// Adapter type name in missing-DSN errors (`"CockroachStore"` /
    /// `"PostgresStore"`). Paired with [`Dialect::DSN_ENV`].
    const STORE_TYPE_NAME: &'static str;

    /// Env var named in missing-DSN errors (`LAMBO_COCKROACH_DSN` /
    /// `LAMBO_POSTGRES_DSN`).
    const DSN_ENV: &'static str;

    /// Label in invalid-DSN errors (`"Cockroach DSN"` / `"Postgres DSN"`).
    const DSN_LABEL: &'static str;

    /// Whether this dialect can log in with a Google OAuth token as the password, which
    /// is Cloud SQL IAM database authentication and therefore PostgreSQL only.
    ///
    /// It gates the `LAMBO_POSTGRES_IAM` opt-in. The variable names Postgres, and a build
    /// carrying both adapters (`ship` does) must not quietly hand a Cloud SQL token to a
    /// Cockroach cluster because one environment variable was exported for the other
    /// store. Defaulted to `false` so a new dialect opts in deliberately.
    const SUPPORTS_CLOUD_SQL_IAM_AUTH: bool = false;

    /// Extra idempotent statements `PgStore`'s `init_schema` runs after
    /// [`Dialect::init_sql`]. Discovered by splitting B0-N4: Cockroach
    /// converges `endpoint STRING` (and `current_token INT`); PostgreSQL
    /// converges `endpoint TEXT` (and `current_token BIGINT`, because
    /// Cockroach `INT` is INT8 and the shared decoder reads i64).
    fn post_init_statements() -> &'static [&'static str];

    /// Dialect-specific session settings applied after the shared
    /// `statement_timeout`. Default: none.
    ///
    /// Discovered by splitting B0-N3: Cockroach sets
    /// `vector_search_beam_size`. PostgreSQL leaves pgvector's
    /// `hnsw.ef_search` at its default (40). Not an ANN-tuning knob row;
    /// B2 ships no knobs.
    fn apply_connect_options(
        options: sqlx::postgres::PgConnectOptions,
    ) -> Result<sqlx::postgres::PgConnectOptions, StoreError> {
        Ok(options)
    }

    /// SQL issued after the contract read and before the vector query, inside
    /// the same search transaction, when `PgStore::with_forced_exact_scan` is
    /// set. Default: none.
    ///
    /// Not a B3 ranking row: it does not change [`Dialect::DISTANCE_OP`] or
    /// [`Dialect::distance_to_score`]. H3's forced-exact lane uses it so
    /// approximation can only come from the index, never from the dialect SQL.
    /// Postgres returns `SET LOCAL enable_indexscan = off`. Cockroach has no
    /// forced-exact lane (H2 measures C-SPANN against an exact oracle instead).
    fn forced_exact_scan_sql() -> Option<&'static str> {
        None
    }
}
