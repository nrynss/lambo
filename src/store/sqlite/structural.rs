//! Read-side graph queries for the SQLite adapter that are not vector search:
//! keyword candidates and the two §4.1 structural queries (`blast_radius`,
//! `interaction_span`), both over one dependents set (#36). Semantics, the errata exclusions and the
//! MemoryStore-exact gating are described in the adapter's module doc
//! ("Case folding" and "Structural queries"); they are locked by the
//! `tests::structural` agreement matrix.

use chrono::{DateTime, Utc};
use sqlx::Row;
use std::time::Duration;

use super::codec::{cutoff_text, db_err, node_id, text_to_ts};
use super::SqliteStore;
use crate::types::{InteractionSpan, NodeId, Scored, SessionId, StoreError};

/// The dependents of one node (#36 read-time dependents semantics, spec §4.1
/// errata): the sources of aged `Dependency` edges into it, plus the targets
/// of its aged `Hierarchical` edges (its children). This is
/// [`crate::types::EdgeType::dependent_end`] as SQL: `Causal`, provenance
/// `Derives`/`Temporal` and association `CoOccurrence`/`Semantic` edges
/// declare no dependence. `UNION` deduplicates a dependent reached by both
/// kinds or by several edges. Binds (`?`, in order): node, session, cutoff,
/// node, session, cutoff. Callers join the result to `concepts` in the
/// session and drop the node itself (self-loop).
///
/// Twin of Postgres/Cockroach's `DEPENDENTS_CTE` in `src/store/pg/sql.rs` and
/// of `MemoryStore::aged_dependents`.
pub(super) const DEPENDENTS_CTE: &str = "dependents AS ( \
     SELECT e.source AS id FROM edges e \
     WHERE e.target = ? AND e.session_id = ? AND e.edge_type = 'Dependency' \
       AND COALESCE(e.event_time, e.created_at) <= ? \
     UNION \
     SELECT e.target AS id FROM edges e \
     WHERE e.source = ? AND e.session_id = ? AND e.edge_type = 'Hierarchical' \
       AND COALESCE(e.event_time, e.created_at) <= ? \
 )";

/// §4.1 blast radius under the #36 errata: the number of `node`'s dependents
/// ([`DEPENDENTS_CTE`]), all of them rather than exclusive ones, that are
/// concepts of the session other than `node`. Binds: the six of
/// `DEPENDENTS_CTE`, then session, node.
pub(super) const BLAST_RADIUS_SQL: &str = "WITH {DEPENDENTS_CTE} \
 SELECT count(*) FROM dependents d \
 JOIN concepts c ON c.id = d.id AND c.session_id = ? \
 WHERE d.id <> ?";

/// §4.1 interaction-span SQL under the #36 errata (twin-shaped with
/// Cockroach's `INTERACTION_SPAN_SQL`; `?` placeholders): the distinct origin
/// interactions of `node`'s dependents ([`DEPENDENTS_CTE`]), other than
/// `node`'s own origin interaction. The span gates on BOTH the dependence
/// edge and the origin-interaction timestamp (spec §4.1 second errata,
/// MemoryStore parity): a dependent is invisible to the span when every edge
/// declaring it is younger than the cutoff, and its interaction is invisible
/// when that interaction is younger than the cutoff. `{DEPENDENTS_CTE}` is
/// substituted at the call site; the substitution keeps this const
/// assertable verbatim in tests. Binds: the six of `DEPENDENTS_CTE`, then
/// session, session, node, cutoff, node, session, session.
///
/// **Own origin (#36).** `node`'s own origin interaction never counts as
/// support: a parent derived together with its children, or an action that
/// depends on a concept minted in the same call, is one interaction vouching
/// for itself.
///
/// **Session scope (F5).** `i.session_id = ?` is not redundant: concepts'
/// `origin_interaction` is a **global** FK, so a concept in session S may
/// legally point at an interaction in session S'. Without the filter the span
/// counted those foreign interactions — inflating `distinct` and (since their
/// timestamps sit outside S's extent) the coverage ratio, both against a
/// session `MemoryStore` never sees.
pub(super) const INTERACTION_SPAN_SQL: &str = "WITH {DEPENDENTS_CTE}, \
 span AS ( \
     SELECT DISTINCT i.id, COALESCE(i.event_time, i.created_at) AS about_ts \
     FROM dependents d \
     JOIN concepts c ON c.id = d.id AND c.session_id = ? \
     JOIN interactions i ON i.id = c.origin_interaction AND i.session_id = ? \
     WHERE d.id <> ? \
       AND COALESCE(i.event_time, i.created_at) <= ? \
       AND NOT EXISTS ( \
           SELECT 1 FROM concepts own \
           WHERE own.id = ? AND own.session_id = ? AND own.origin_interaction = i.id) \
 ), \
 extent AS ( \
     SELECT min(COALESCE(event_time, created_at)) AS lo, \
            max(COALESCE(event_time, created_at)) AS hi \
     FROM interactions WHERE session_id = ? \
 ) \
 SELECT \
     (SELECT count(*) FROM span), \
     (SELECT min(about_ts) FROM span), \
     (SELECT max(about_ts) FROM span), \
     extent.lo, extent.hi \
 FROM extent";

/// Expand `{DEPENDENTS_CTE}` in one of the structural queries above.
pub(super) fn with_dependents(sql: &str) -> String {
    sql.replace("{DEPENDENTS_CTE}", DEPENDENTS_CTE)
}

impl SqliteStore {
    /// Mirror MemoryStore: queries against a session that was never written
    /// fail with `SessionNotFound`, not an empty answer.
    pub(super) async fn require_session(&self, session: &SessionId) -> Result<(), StoreError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM sessions WHERE session_id = ?")
            .bind(&session.0)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err("lookup session", e))?;
        if found.is_none() {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        Ok(())
    }

    pub(super) async fn select_keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // MemoryStore parity: trim/lowercase, drop empties; empty tokens or
        // limit 0 match nothing (a bare `contains("")` would match everything).
        let tokens_l: Vec<String> = tokens
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens_l.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.require_session(session).await?;

        // Exact substring semantics (memory's `contains`) via instr() on
        // lowercased content/key — no LIKE wildcard interpretation. Score =
        // number of tokens hitting content OR canonical_key. Ties: canonical
        // key asc, then id (issue #2; SQLite's default BINARY collation
        // compares the same UTF-8 bytes Rust's `str` ordering does, so this
        // matches MemoryStore's Rust-side tie-break).
        let mut sql = String::from("SELECT id, ");
        for (i, _) in tokens_l.iter().enumerate() {
            if i > 0 {
                sql.push_str(" + ");
            }
            sql.push_str("(instr(lower(content), ?) > 0 OR instr(lower(canonical_key), ?) > 0)");
        }
        sql.push_str(" AS score FROM concepts WHERE session_id = ? AND (");
        for (i, _) in tokens_l.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("(instr(lower(content), ?) > 0 OR instr(lower(canonical_key), ?) > 0)");
        }
        sql.push_str(") ORDER BY score DESC, canonical_key ASC, id ASC LIMIT ?");

        let mut q = sqlx::query(&sql);
        for tok in &tokens_l {
            q = q.bind(tok).bind(tok);
        }
        q = q.bind(&session.0);
        for tok in &tokens_l {
            q = q.bind(tok).bind(tok);
        }
        q = q.bind(limit as i64);

        let rows = q
            .fetch_all(self.pool())
            .await
            .map_err(|e| db_err("keyword_candidates", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row
                .try_get(0)
                .map_err(|e| db_err("keyword_candidates", e))?;
            let score: i64 = row
                .try_get(1)
                .map_err(|e| db_err("keyword_candidates", e))?;
            out.push(Scored::new(node_id(&id, "concept id")?, score as f64));
        }
        Ok(out)
    }

    pub(super) async fn count_blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        // Spec §4.1 with the #36 errata ported to `?` placeholders; the cutoff
        // is computed in Rust (SQLite has no INTERVAL) and bound as the fixed
        // ISO-8601 TEXT. Edges age on their about-time (D fallback rule:
        // `COALESCE(e.event_time, e.created_at)`), exactly like MemoryStore.
        // Dependents are scoped to concepts of this session and the edges to
        // the session's own rows, as MemoryStore's per-session snapshot is.
        self.require_session(session).await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff_text(now, min_edge_age)?;
        let node_text = node.0.to_string();

        let row = sqlx::query(&with_dependents(BLAST_RADIUS_SQL))
            .bind(&node_text)
            .bind(&session.0)
            .bind(&cutoff)
            .bind(&node_text)
            .bind(&session.0)
            .bind(&cutoff)
            .bind(&session.0)
            .bind(&node_text)
            .fetch_one(self.pool())
            .await
            .map_err(|e| db_err("blast_radius", e))?;
        let n: i64 = row.try_get(0).map_err(|e| db_err("blast_radius", e))?;
        Ok(n as u64)
    }

    pub(super) async fn select_interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        // Spec §4.1 span query with the #36 errata: distinct origin
        // interactions of `node`'s dependents other than its own, aged on BOTH
        // the dependence edge and the origin interaction (MemoryStore
        // agreement). Coverage is computed in Rust in ms, identical to
        // MemoryStore's formula.
        self.require_session(session).await?;
        // F8: the cutoff anchor is the caller's `now`, never a wall clock here.
        let cutoff = cutoff_text(now, min_age)?;
        let node_text = node.0.to_string();

        let row = sqlx::query(&with_dependents(INTERACTION_SPAN_SQL))
            .bind(&node_text)
            .bind(&session.0)
            .bind(&cutoff)
            .bind(&node_text)
            .bind(&session.0)
            .bind(&cutoff)
            .bind(&session.0)
            .bind(&session.0)
            .bind(&node_text)
            .bind(&cutoff)
            .bind(&node_text)
            .bind(&session.0)
            .bind(&session.0)
            .fetch_one(self.pool())
            .await
            .map_err(|e| db_err("interaction_span", e))?;

        let distinct: i64 = row.try_get(0).map_err(|e| db_err("interaction_span", e))?;
        let span_lo: Option<String> = row.try_get(1).map_err(|e| db_err("interaction_span", e))?;
        let span_hi: Option<String> = row.try_get(2).map_err(|e| db_err("interaction_span", e))?;
        let sess_lo: Option<String> = row.try_get(3).map_err(|e| db_err("interaction_span", e))?;
        let sess_hi: Option<String> = row.try_get(4).map_err(|e| db_err("interaction_span", e))?;

        let coverage = match (span_lo, span_hi) {
            (Some(lo_s), Some(hi_s)) => {
                let lo = text_to_ts(&lo_s)?;
                let hi = text_to_ts(&hi_s)?;
                let sess_lo = match sess_lo {
                    Some(s) => text_to_ts(&s)?,
                    None => lo,
                };
                let sess_hi = match sess_hi {
                    Some(s) => text_to_ts(&s)?,
                    None => hi,
                };
                let sess_span = (sess_hi - sess_lo).num_milliseconds().max(0) as f64;
                if sess_span <= 0.0 {
                    // F1: single-point session extent (one interaction, or all
                    // interactions sharing a timestamp) with at least one
                    // supported interaction (span_lo/hi are Some here, so
                    // distinct >= 1) -> coverage 1.0, mirroring MemoryStore
                    // and the Cockroach SQL (canonization Stage 2 parity).
                    1.0
                } else {
                    let span = (hi - lo).num_milliseconds().max(0) as f64;
                    (span / sess_span).clamp(0.0, 1.0)
                }
            }
            _ => 0.0,
        };
        Ok(InteractionSpan {
            distinct: distinct as u64,
            coverage,
        })
    }
}
