//! Pure-logic unit tests for the Cockroach dialect (no cluster), grouped by
//! subject.

use super::*;
use chrono::TimeZone;

mod codecs;
mod config;
mod queries;
mod schema;
mod sql_shapes;

/// The Cockroach instantiation of [`DialectSql`], for the SQL-shape tests: the
/// statements in the adapter are exactly what `PgStore<CockroachDialect>`
/// issues, so the tests still read the real text rather than a re-spelled copy
/// of it.
fn crdb_sql() -> DialectSql {
    DialectSql::for_dialect::<CockroachDialect>()
}
