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
//! [`StoreAndUnflushedSource`] (#60) is the fourth, for a Postgres-family
//! holder's derive: the store for what the flush made durable, plus only the
//! holder's unflushed concepts ranked here.
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

use std::collections::HashSet;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::graph::Graph;
use crate::store::vector_source::{ensure_is_an_embedding, rank_by_cosine, VectorCandidateSource};
use crate::store::{validate_vector_candidate_limit, GraphStore, MAX_VECTOR_CANDIDATE_LIMIT};
use crate::types::{Concept, EmbeddingContract, Node, NodeId, Scored, SessionId, StoreError};

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
    let Some(dim) = checked_scan_width(graph, session, probe, expected_contract, limit)? else {
        return Ok(Vec::new());
    };
    #[cfg(test)]
    WHOLE_GRAPH_SCANS.with(|n| n.set(n.get() + 1));
    rank_concepts(session, probe, dim, graph.concepts(), limit)
}

#[cfg(test)]
thread_local! {
    /// Whole-graph scans [`graph_vector_candidates`] ran on this thread
    /// (tests only: #60 review L3 counts them).
    static WHOLE_GRAPH_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many whole-graph scans this thread has run (tests only).
#[cfg(test)]
pub(crate) fn whole_graph_scans() -> usize {
    WHOLE_GRAPH_SCANS.with(std::cell::Cell::get)
}

/// Steps 1 to 4 of [`graph_vector_candidates`]: the checks every graph-side
/// read applies before it ranks. `Ok(None)` is "nothing to rank" (`limit` 0,
/// or no embedding contract yet); `Ok(Some(dim))` is the contract's width.
fn checked_scan_width(
    graph: &Graph,
    session: &SessionId,
    probe: &[f32],
    expected_contract: &EmbeddingContract,
    limit: usize,
) -> Result<Option<usize>, StoreError> {
    validate_vector_candidate_limit(limit)?;
    if limit == 0 {
        return Ok(None);
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
        return Ok(None);
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
    Ok(Some(stored.dim))
}

/// Step 5: score every concept in `concepts` that carries a vector.
fn rank_concepts<'g>(
    session: &SessionId,
    probe: &[f32],
    dim: usize,
    concepts: impl IntoIterator<Item = &'g Concept>,
    limit: usize,
) -> Result<Vec<Scored<NodeId>>, StoreError> {
    let mut candidates = Vec::new();
    for concept in concepts {
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

/// The concept `id` names in `graph`, if it is one.
fn concept_of<'g>(graph: &'g Graph, id: &NodeId) -> Option<&'g Concept> {
    match graph.node(*id) {
        Some(Node::Concept(c)) => Some(c),
        _ => None,
    }
}

/// The Postgres family's holder derive source (#60): the store's checked
/// read for what the flush has made durable, plus an exact scan of only the
/// holder's unflushed concepts ([`Graph::unflushed_ids`]).
///
/// # Why a union, not the whole graph
///
/// The store's search sees only flushed rows, and the flush lags by seconds
/// to minutes, so asking it alone turned paraphrases derived seconds apart
/// into permanent near-duplicates. Ranking every graph vector fixed that but
/// cost O(n * dim) per probe, synchronously, on the backend recommended for
/// large sessions: 98 ms per probe at 100,000 concepts at d = 1024, about
/// 6 s for a 64-concept derive (release, measured for #60; see
/// `dev-diary/notes/feature-8-vector-source.md`). The union costs the
/// pre-#60 indexed database query plus O(unflushed), about 0.8 ms per probe
/// with 1,000 concepts unflushed whatever the session size, and misses
/// nothing the store could see or the holder wrote since:
///
/// * the graph's unflushed set is cleared only after the flush commit was
///   acknowledged, and a dropped batch stays in it, so every concept is in
///   the database, in the set, or both;
/// * the set is read under the read lock **before** the database is asked,
///   so a concept committed and cleared in between is already readable
///   there. (Reading it after the query could miss exactly that concept.)
///
/// # How the answer is formed
///
/// 1. Under the read lock: the graph-side checks of
///    [`graph_vector_candidates`], then the top `limit` of the unflushed
///    concepts by exact cosine.
/// 2. No lock: the store's checked read, for `limit` plus the size of the
///    unflushed set, so the rows the graph overrides (a pending delete, a
///    rewritten vector) cannot crowd a durable candidate out.
/// 3. Under the read lock: every id from either list that is still a concept
///    with a vector is re-scored by [`rank_by_cosine`] on the graph's own
///    vector, and the best `limit` are returned. The graph is the authority:
///    a row it deleted is dropped, a row it rewrote is scored by the new
///    vector, and the scores are exact cosine on both legs.
///
/// Two cases rank every graph vector instead (exact, as the graph source
/// does): the session's embedding contract has a change the store has not
/// seen (a fresh session before its first flush, a re-embed), so the store
/// would compare against the old one; or more writes are unflushed than the
/// store's read can over-fetch for ([`MAX_VECTOR_CANDIDATE_LIMIT`]), which
/// takes a store outage or a degraded session.
///
/// What remains different from ranking the whole graph: the durable leg's
/// pool is the store's own top-k, by its distance and possibly from an
/// approximate index, so a durable concept the index misses is missed here
/// too, as it was before #60. The final scores are exact cosine.
#[derive(Clone, Copy)]
pub(crate) struct StoreAndUnflushedSource<'a> {
    store: &'a dyn GraphStore,
    graph: &'a RwLock<Graph>,
}

impl<'a> StoreAndUnflushedSource<'a> {
    pub(crate) fn new(store: &'a dyn GraphStore, graph: &'a RwLock<Graph>) -> Self {
        Self { store, graph }
    }

    /// The store this source asks for the durable leg.
    pub(crate) fn store(&self) -> &'a dyn GraphStore {
        self.store
    }
}

/// What step 1 hands step 2.
enum UnionPlan {
    /// The answer is already known (nothing to rank, or the whole graph was
    /// ranked).
    Done(Vec<Scored<NodeId>>),
    /// Ask the store for `store_limit`, then re-rank with `unflushed`.
    AskStore {
        unflushed: Vec<NodeId>,
        store_limit: usize,
    },
}

#[async_trait]
impl VectorCandidateSource for StoreAndUnflushedSource<'_> {
    async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // Step 1, synchronous under the read lock, released before the await.
        let plan = {
            let graph = self.graph.read();
            match checked_scan_width(&graph, session, probe, expected_contract, limit)? {
                None => UnionPlan::Done(Vec::new()),
                Some(dim) => {
                    let store_limit = limit.saturating_add(graph.unflushed_len());
                    if graph.contract_unflushed() || store_limit > MAX_VECTOR_CANDIDATE_LIMIT {
                        UnionPlan::Done(rank_concepts(
                            session,
                            probe,
                            dim,
                            graph.concepts(),
                            limit,
                        )?)
                    } else {
                        let unflushed = rank_concepts(
                            session,
                            probe,
                            dim,
                            graph
                                .unflushed_ids()
                                .filter_map(|id| concept_of(&graph, id)),
                            limit,
                        )?;
                        UnionPlan::AskStore {
                            unflushed: unflushed.into_iter().map(|s| s.item).collect(),
                            store_limit,
                        }
                    }
                }
            }
        };
        let (unflushed, store_limit) = match plan {
            UnionPlan::Done(hits) => return Ok(hits),
            UnionPlan::AskStore {
                unflushed,
                store_limit,
            } => (unflushed, store_limit),
        };

        // Step 2, no lock held.
        let durable = self
            .store
            .vector_candidates_checked(session, probe, expected_contract, store_limit)
            .await?;

        // Step 3, synchronous under the read lock.
        let graph = self.graph.read();
        let Some(dim) = checked_scan_width(&graph, session, probe, expected_contract, limit)?
        else {
            return Ok(Vec::new());
        };
        let mut seen = HashSet::with_capacity(unflushed.len() + durable.len());
        let pool = unflushed
            .iter()
            .chain(durable.iter().map(|s| &s.item))
            .filter(|id| seen.insert(**id))
            .filter_map(|id| concept_of(&graph, id));
        rank_concepts(session, probe, dim, pool, limit)
    }
}
