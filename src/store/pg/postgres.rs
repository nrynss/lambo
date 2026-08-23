//! PostgreSQL + pgvector dialect of the Postgres-wire-protocol family.
//!
//! Feature: `store-postgres` (same `sqlx` postgres driver as `store-cockroach`;
//! no second driver). Registered in [`crate::store::build_store`] for
//! [`crate::store::StoreKind::Postgres`].
//!
//! # Fail closed, naming B2
//!
//! B1's job is that the kind can **name** a dialect. B2 owns templated width,
//! hnsw-from-init, and the distance conversion. This file therefore does **not**
//! copy Cockroach SQL: a stub that promotes nothing, or a dialect that speaks
//! Cockroach, is indistinguishable from a finished broken Postgres (C1's
//! recorded precedent). Construction, `init_sql`, and `vector_dim` return a
//! hard error that names B2. `distance_to_score` panics if called: that row
//! cannot return `Result`, and a guessed formula would silently mis-rank.

use std::borrow::Cow;

use super::{Dialect, PgStore};
use crate::store::{postgres_not_ready_msg, StoreConfig, StoreError};

/// PostgreSQL + pgvector, as a [`Dialect`] of the Postgres-wire-protocol family.
///
/// A zero-sized compile-time selector: it is never constructed, only named as
/// `PgStore<PostgresDialect>`. B1 introduces the name; B2 fills the DDL.
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn init_sql(_dim: usize) -> Result<Cow<'static, str>, StoreError> {
        Err(StoreError::Backend(postgres_not_ready_msg("init a schema")))
    }

    /// PostgreSQL's text type is `TEXT`. Not copied from Cockroach's `STRING`:
    /// if this token ever reached a statement before B2, Cockroach would reject
    /// it rather than run the wrong dialect silently. B3's table is the
    /// authority for the spelling.
    const STRING_CAST: &'static str = "::TEXT";

    /// pgvector's dense-vector type is `vector`. Not Cockroach's `VECTOR`.
    const VECTOR_CAST: &'static str = "::vector";

    /// `<=>` is pgvector cosine **distance**. Not Cockroach's `<->` (L2). Using
    /// the Cockroach operator on Postgres would compile and rank by the wrong
    /// metric; using this operator on Cockroach fails at the first vector query.
    const DISTANCE_OP: &'static str = "<=>";

    /// B3 owns this conversion. A stub formula would silently mis-rank, which
    /// is worse than refusing: it would read as a finished dialect. The
    /// [`Dialect::init_sql`] / [`Dialect::vector_dim`] errors mean a store of
    /// this type cannot be constructed today, so this is a backstop.
    fn distance_to_score(_dist: f64) -> f64 {
        unimplemented!(
            "PostgresDialect::distance_to_score is B3: <=> is cosine distance \
             and the score is 1 - d; a stub formula would silently mis-rank"
        )
    }

    fn vector_dim(_cfg: &StoreConfig) -> Result<usize, StoreError> {
        Err(StoreError::Backend(postgres_not_ready_msg(
            "report a vector width",
        )))
    }
}

/// The durable PostgreSQL adapter, under the name `build_store` will use once
/// B2 lands a working [`PostgresDialect::init_sql`]. Today [`PgStore::new`]
/// fails through [`Dialect::vector_dim`] / [`Dialect::init_sql`].
pub type PostgresStore = PgStore<PostgresDialect>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{build_store, StoreConfig, StoreKind};

    #[test]
    fn dialect_tokens_are_not_cockroach_sql() {
        assert_ne!(
            PostgresDialect::STRING_CAST,
            "::STRING",
            "copying Cockroach STRING would make a leftover kind=postgres \
             pointed at Cockroach run instead of failing loud"
        );
        assert_ne!(PostgresDialect::VECTOR_CAST, "::VECTOR");
        assert_ne!(
            PostgresDialect::DISTANCE_OP,
            "<->",
            "copying Cockroach L2 would silently mis-rank on pgvector"
        );
    }

    #[test]
    #[should_panic(expected = "B3")]
    fn distance_to_score_does_not_guess_a_formula() {
        let _ = PostgresDialect::distance_to_score(0.5);
    }

    #[test]
    fn init_sql_and_vector_dim_name_b2() {
        let err = PostgresDialect::init_sql(1024).unwrap_err().to_string();
        assert!(err.contains("B2"), "{err}");
        assert!(err.contains("postgres"), "{err}");
        let err = PostgresDialect::vector_dim(&StoreConfig::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("B2"), "{err}");
    }

    #[test]
    fn postgres_store_new_fails_closed_even_with_a_dsn() {
        let Err(err) = PostgresStore::new(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some("postgres://u@localhost/lambo".into()),
            path: None,
            vector_dim: None,
        }) else {
            panic!("PostgresStore::new must fail closed in B1");
        };
        let err = err.to_string();
        assert!(err.contains("B2"), "{err}");
        assert!(
            !err.to_ascii_lowercase().contains("cockroachstore requires"),
            "must not look like a Cockroach DSN miss: {err}"
        );
    }

    #[test]
    fn build_store_fails_closed_naming_b2() {
        let Err(err) = build_store(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some("postgres://u@localhost/lambo".into()),
            path: None,
            vector_dim: None,
        }) else {
            panic!("postgres must not construct a working adapter in B1");
        };
        let err = err.to_string();
        assert!(err.contains("B2"), "{err}");
        assert!(err.contains("cockroach"), "{err}");
        assert!(
            !err.to_ascii_lowercase().contains("memory store"),
            "no silent fallback: {err}"
        );
        assert!(StoreKind::Postgres.is_compiled());
        assert!(
            !StoreKind::Postgres.is_ready(),
            "is_ready is false until B2 lands a working dialect"
        );
    }
}
