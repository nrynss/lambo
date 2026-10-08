//! The graph-backed vector candidate source (#8).
//!
//! Spec §2.1 makes the in-memory graph primary and the durable store a copy
//! synchronised behind it. The session holder's graph already carries every
//! concept vector (`Concept.embedding`, decoded once at session load and
//! attached to each new concept as it is derived), so the holder can rank its
//! own vectors instead of asking the store to read and decode them again on
//! every recall and once per unmatched concept on every hybrid derive.
//!
//! [`GraphVectorSource`] is that third implementation of
//! [`VectorCandidateSource`], beside the SQLite scan and the pg-family query.
//! It is an exact scan over [`Graph::concepts`] scored by the shared
//! [`rank_by_cosine`], so on a store whose own checked read is the same exact
//! scan (`GraphStore::exact_vector_scan`) it returns the same candidates in the
//! same order with the same scores, bit for bit: the stored codec round-trips
//! `f32` exactly (CON-8), so the graph and the store hold identical bits, and
//! the scorer and its tie-break are the same function.
//!
//! **What it keeps: nothing.** No matrix, index or cache sits beside the
//! graph. The scan borrows the vectors the concepts already own, under the
//! caller-visible graph read lock, so there is no second copy to keep in step
//! with stamping, re-embedding, backfill, `remove_node`, GC or session erasure:
//! whatever the graph holds when the lock is taken is what is ranked.
//!
//! **Where it differs from the store's read, deliberately.** The graph is
//! fresher than the store. A concept derived and not yet flushed is a
//! candidate here and was invisible to the store's scan; one removed and not
//! yet flushed is gone here and was still returned there (recall skipped it
//! at assembly). The embedding contract checked is the graph's, which is the
//! durable one plus whatever the next flush will write. Over a flushed graph
//! the two answers are identical.
//!
//! **Lock discipline (spec §6.4).** The read lock is taken and released inside
//! one synchronous call ([`graph_vector_candidates`] runs under it); no
//! `.await` happens while it is held. Callers reach this source only from
//! phases that hold no graph lock (recall's gather, hybrid derive's gather).

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::graph::Graph;
use crate::store::validate_vector_candidate_limit;
use crate::store::vector_source::{ensure_is_an_embedding, rank_by_cosine, VectorCandidateSource};
use crate::types::{EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// Ranks the vectors the session holder's graph already holds.
///
/// Borrowed rather than owning an `Arc`: the value lives for one call, inside
/// the `VectorCandidates` a recall or derive is handed.
#[derive(Clone, Copy)]
pub(crate) struct GraphVectorSource<'a> {
    graph: &'a RwLock<Graph>,
}

impl<'a> GraphVectorSource<'a> {
    pub(crate) fn new(graph: &'a RwLock<Graph>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl VectorCandidateSource for GraphVectorSource<'_> {
    async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // Synchronous under the read lock, released before this returns.
        let graph = self.graph.read();
        graph_vector_candidates(&graph, session, probe, expected_contract, limit)
    }
}

/// The checked candidate read over one graph, under the contract every
/// [`VectorCandidateSource`] shares, in the order SQLite's checked read applies
/// it so the same input meets the same error at the same step:
///
/// 1. `limit` is validated against the public bound; `0` returns nothing.
/// 2. The probe must be an embedding (finite, non-zero norm).
/// 3. A graph with no embedding contract returns nothing (SQLite: no durable
///    contract). A contract incompatible with `expected_contract` is refused
///    with [`StoreError::Invariant`], with SQLite's message, which recall
///    recognises to annotate the result as keyword-only.
/// 4. A probe whose width is not the contract's is refused with
///    [`StoreError::Invariant`].
/// 5. Every concept carrying a vector is scored by [`rank_by_cosine`]. A
///    vector whose width disagrees with the contract is reported as
///    [`StoreError::Backend`], as SQLite reports a corrupt row (the graph's
///    write gates make it unreachable; the check costs one comparison).
///
/// The graph holds exactly one session. A request for another session is a
/// caller bug (recall and hybrid derive both take the session from the graph)
/// and is refused rather than answered with an empty list that would hide it.
pub(crate) fn graph_vector_candidates(
    graph: &Graph,
    session: &SessionId,
    probe: &[f32],
    expected_contract: &EmbeddingContract,
    limit: usize,
) -> Result<Vec<Scored<NodeId>>, StoreError> {
    validate_vector_candidate_limit(limit)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    ensure_is_an_embedding(probe)?;
    if session != graph.session_id() {
        return Err(StoreError::Invariant(format!(
            "graph-backed vector candidates asked for session {} but the graph holds session {}",
            session.0,
            graph.session_id().0
        )));
    }
    let Some(stored) = graph.embedding() else {
        return Ok(Vec::new());
    };
    stored.ensure_compatible(expected_contract).map_err(|err| {
        StoreError::Invariant(format!(
            "vector candidate lookup refused after embedding contract changed: {err}"
        ))
    })?;
    // Word for word SQLite's refusal: the parity tests compare messages. The
    // holder's graph carries the session contract it loaded from, and
    // flushes to, the store.
    if probe.len() != stored.dim {
        return Err(StoreError::Invariant(format!(
            "query embedding has {} dimensions but session {} stores vectors of {} \
             (the session's durable embedding contract is the authority here, not \
             the process-wide vector_dimensions())",
            probe.len(),
            session.0,
            stored.dim
        )));
    }
    let dim = stored.dim;
    let mut candidates = Vec::new();
    for concept in graph.concepts() {
        let Some(vector) = concept.embedding.as_deref() else {
            continue;
        };
        if vector.len() != dim {
            return Err(StoreError::Backend(format!(
                "concept {} carries a vector of {} dimensions but session {} declares {dim}",
                concept.id,
                vector.len(),
                session.0
            )));
        }
        candidates.push((concept.id, vector, concept.canonical_key.as_str()));
    }
    Ok(rank_by_cosine(probe, candidates, limit))
}
