//! The `Dialect` trait: everything [`super::PgStore`] cannot say the same way
//! on every Postgres-wire-protocol engine.
//!
//! **Compile-time, monomorphized, no dynamic dispatch.** `PgStore<D: Dialect>`
//! takes the dialect as a type parameter and every method below is an
//! associated const or an associated function, so a dialect call costs nothing
//! at runtime and a missing dialect is a compile error rather than a panic.
//!
//! **The surface is deliberately tiny, and deliberately closed.** It is exactly
//! the table in `dev-diary/lambo-for-mooshik/B-postgres-store.md` §B3: the DDL,
//! two casts, the distance operator and its score conversion, and the width
//! authority. Nothing else belongs here yet. The shared subset of two SQL
//! adapters is *discovered* by diffing two real implementations, not guessed
//! from one, so a method added because PostgreSQL "might need it" would be a
//! guess with a trait's authority. B0 ships one dialect; B2 adds the second and
//! the diff between them is what may widen this trait.
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
}
