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
//! to the SQLite scan's (the CON-8 text codec round-trips `f32` exactly).
//!
//! **The caller side** (#27) is [`VectorCandidates`]: recall's vector leg
//! (`recall::candidates::gather_from`) and hybrid derive's semantic match
//! (`graph::hybrid::derive_with`) reach candidates only through the value they
//! are handed, never by calling the store. The owners that hand it out are
//! `Memory::vector_candidates` and `WriteCtx::vector_candidates`; #8 changes
//! what those two return (a graph-backed variant here), and nothing else.
//!
//! The module compiles in every build, the Memory-only default included, so
//! #8's graph-backed source can implement the trait and call the scorer
//! wherever the graph runs; until it lands, builds without a SQL adapter
//! carry both unused, hence the `dead_code` allowances.
//!
//! The stored vector codec is not part of this seam: it lives in
//! `store/vector.rs` (`encode_vector` / `decode_vector` and SQLite's BLOB framing).

use async_trait::async_trait;

use crate::store::{Capabilities, GraphStore};
use crate::types::{tie_break_by_key, EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// Where a caller's vector candidates come from (#27, caller side).
///
/// Recall and hybrid derive are given one of these instead of a store, and ask
/// it two things: whether a vector leg exists at all ([`Self::available`], no
/// I/O), and the checked candidates for a probe ([`Self::checked`]). Today the
/// only source is the durable store, with exactly the behaviour the callers
/// had when they called it directly: the capability bit is
/// `Capabilities::VECTOR_SEARCH`, and the read is
/// `GraphStore::vector_candidates_checked`, capability refusal included. #8
/// adds a graph-backed variant (a [`VectorCandidateSource`] over the in-memory
/// graph) and the callers do not change.
///
/// An enum rather than a trait object so the store path stays statically
/// dispatched.
#[derive(Clone, Copy)]
pub(crate) enum VectorCandidates<'a> {
    /// The durable store's checked read.
    Store(&'a dyn GraphStore),
}

impl<'a> VectorCandidates<'a> {
    /// The store as the source: today's behaviour.
    pub(crate) fn from_store(store: &'a dyn GraphStore) -> Self {
        Self::Store(store)
    }

    /// Whether the vector leg can run at all. Synchronous and I/O-free, so a
    /// caller can skip the query embed when it cannot.
    pub(crate) fn available(&self) -> bool {
        match self {
            Self::Store(store) => store.capabilities().contains(Capabilities::VECTOR_SEARCH),
        }
    }

    /// Checked candidates for `probe`, under the contract of
    /// [`VectorCandidateSource::checked_vector_candidates`].
    pub(crate) async fn checked(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        match self {
            Self::Store(store) => {
                store
                    .vector_candidates_checked(session, probe, expected_contract, limit)
                    .await
            }
        }
    }
}

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

/// The precondition `store::vector::encode_vector` enforces, callable on its
/// own by a source that does **not** encode the vector it is about to use.
///
/// # Why this is separate (B-E2E-R2-3)
///
/// E2E-F9 put the zero-norm refusal in the codec because that is where all
/// three sqlx adapters meet. That covers every write path, and it covers the
/// **query** path on the pg family only because those adapters encode the
/// probe before binding it. SQLite never encodes its probe: it hands it to
/// `rank_by_cosine`, and [`crate::embed::cosine`] clamps the denominator with
/// `.max(1e-12)`, so a zero probe scored every row a plausible `0.0` and
/// returned candidates in tie-break order. One contract-violating input, a
/// loud refusal on Postgres and a silent meaningless ranking on SQLite, which
/// is the exact sentence E2E-F9 was filed under.
///
/// So SQLite's `vector_candidates_checked` calls this at the same point in the
/// sequence where the pg family encodes its probe: after the limit checks,
/// before the store is read. Same input, same error, same place, all three
/// adapters.
///
/// Non-finite elements are refused here too, not only zero norms: that is the
/// other half of what encoding the probe was implicitly enforcing on the pg
/// family, and leaving it out would close half of one divergence and keep the
/// other.
#[cfg_attr(
    not(any(
        feature = "store-cockroach",
        feature = "store-postgres",
        feature = "store-sqlite"
    )),
    allow(dead_code)
)]
pub fn ensure_is_an_embedding(v: &[f32]) -> Result<(), StoreError> {
    if let Some(bad) = v.iter().find(|x| !x.is_finite()) {
        return Err(StoreError::Backend(format!(
            "embedding contains non-finite value {bad} (at index {:?})",
            v.iter().position(|x| !x.is_finite())
        )));
    }
    if !v.is_empty() {
        // Accumulated in f32 on purpose: this is the arithmetic pgvector and
        // Cockroach do, so a vector whose norm underflows to zero for them is
        // refused here rather than becoming a NaN score there.
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        // `<= 0.0 || is_nan()` rather than `!(norm_sq > 0.0)`: exactly the same
        // set of refused values, without the negated partial-ord comparison
        // clippy refuses under -D warnings.
        if norm_sq <= 0.0 || norm_sq.is_nan() {
            return Err(StoreError::Backend(format!(
                "embedding has zero norm over {} dimensions, which violates the unit-norm \
                 output contract of Embedder::embed. A vector with no direction has no \
                 cosine to anything: pgvector scores it NaN against every row and \
                 CockroachDB scores it a flat 0.5, so the same data would rank differently \
                 on the two stores. Refusing to write or query it",
                v.len(),
            )));
        }
    }
    Ok(())
}
