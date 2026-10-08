//! The vector candidate seam (#26, prepared for #8).
//!
//! Vector candidate selection has one narrow interface, [`VectorCandidateSource`]:
//! a session, a query vector, the embedding contract the caller expects, and a
//! limit go in; scored node ids, best first, come out. It knows nothing about SQL
//! rows, the stored vector codec or the `GraphStore` surface around it.
//!
//! Implementations today, each in its backend's `vector_candidates` module:
//!
//! * `SqliteStore` — an exact scan: selection reads every stored vector of the
//!   session (`select_session_vectors`), scoring is [`rank_by_cosine`] below.
//! * `PgStore<D>` (PostgreSQL, CockroachDB) — the database ranks by distance
//!   (`D::distance_to_score`), with the DECISION D1 global fetch and the exact
//!   session fallback.
//!
//! `GraphStore::vector_candidates_checked` on both adapters delegates here, and
//! every implementation keeps the contract check and the candidate read in one
//! transaction, so a contract change cannot interleave with the read.
//!
//! #8 adds a graph-backed implementation that ranks against the vectors the
//! in-memory graph already holds. It implements this trait without implementing
//! `GraphStore`, and it reuses [`rank_by_cosine`] so its ranking is bit-identical
//! to the SQLite scan's (the CON-8 text codec round-trips `f32` exactly). The
//! caller side of the seam (recall's vector leg and derive's semantic match
//! reaching candidates through a source they are given) is #27's.
//!
//! The module compiles in every build, the Memory-only default included, so
//! #8's graph-backed source can implement the trait and call the scorer
//! wherever the graph runs; until it lands, builds without a SQL adapter
//! carry both unused, hence the `dead_code` allowances.
//!
//! The stored vector codec is not part of this seam: it lives in
//! `store/vector.rs` (`encode_vector` / `decode_vector` and SQLite's BLOB framing).

use async_trait::async_trait;

use crate::types::{tie_break_by_key, EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// Selects and scores vector candidates for one session.
///
/// Contract shared by every implementation, as `GraphStore::vector_candidates_checked`
/// documents it: `limit` is validated against the public bound and `0` returns
/// nothing; an unknown session, a session with no durable contract, or one with no
/// vectors yields an empty list; a durable contract incompatible with
/// `expected_contract` is refused with [`StoreError::Invariant`]; results are
/// ordered score descending with the issue-2 tie-break (canonical key, then id).
#[async_trait]
#[cfg_attr(
    not(any(
        feature = "store-cockroach",
        feature = "store-postgres",
        feature = "store-sqlite"
    )),
    allow(dead_code)
)]
pub(crate) trait VectorCandidateSource: Send + Sync {
    async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;
}

/// One decoded stored vector, before scoring, with the concept's canonical key
/// riding along (the issue-2 tie-break consumes it on exact score ties).
#[cfg(feature = "store-sqlite")]
pub(crate) type VectorCandidate = (NodeId, Vec<f32>, String);

/// **Candidate scoring**, the fixed half. Exact cosine, best first; ties
/// broken by canonical key ascending, then the smaller node id
/// ([`tie_break_by_key`], issue #2), so the answer is deterministic across
/// runs as well as within one (MemoryStore / Cockroach parity). Stays exact
/// whatever candidate selection becomes: an approximate index would
/// prune the pool, never the ranking.
///
/// Candidates are borrowed, so a source that already holds its vectors (#8's
/// graph-backed source over the in-memory graph) scores them in place
/// instead of copying every vector and key per call. The sort is stable, so
/// candidates that compare equal keep their input order.
#[cfg_attr(not(feature = "store-sqlite"), allow(dead_code))]
pub(crate) fn rank_by_cosine<'a>(
    probe: &[f32],
    candidates: impl IntoIterator<Item = (NodeId, &'a [f32], &'a str)>,
    limit: usize,
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<(Scored<NodeId>, &str)> = candidates
        .into_iter()
        .map(|(id, vector, key)| {
            (
                Scored::new(id, f64::from(crate::embed::cosine(probe, vector))),
                key,
            )
        })
        .collect();
    scored.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
    scored.truncate(limit);
    scored
}
