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
//! * `GraphVectorSource` (#8, `graph::vector_source`) — the session holder's
//!   own graph: an exact scan over the vectors its concepts already carry,
//!   scored by [`rank_by_cosine`]. It implements this trait without
//!   implementing `GraphStore`, and its ranking is bit-identical to the SQLite
//!   scan's (the CON-8 text codec round-trips `f32` exactly).
//! * `StoreAndUnflushedSource` (#60, `graph::vector_source`) — a Postgres
//!   family holder's derive: the store's checked read for what the flush has
//!   made durable, plus an exact scan of only the holder's unflushed
//!   concepts, every candidate re-scored on the graph's vector.
//!
//! **The caller side** (#27) is [`VectorCandidates`]: recall's vector leg
//! (`recall::candidates::gather_from`) and hybrid derive's semantic match
//! (`graph::hybrid::derive_with`) reach candidates only through the value they
//! are handed, never by calling the store. The owners that hand it out are
//! `Memory::vector_candidates` and `WriteCtx::vector_candidates`, and both
//! build it with [`VectorCandidates::for_holder`], the one place the choice
//! between the store and the holder's graph is made (#8).
//!
//! The module compiles in every build, the Memory-only default included: the
//! graph-backed source implements the trait and calls the scorer wherever the
//! graph runs.
//!
//! The stored vector codec is not part of this seam: it lives in
//! `store/vector.rs` (`encode_vector` / `decode_vector` and SQLite's BLOB framing).

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::graph::vector_source::{GraphVectorSource, StoreAndUnflushedSource};
use crate::graph::Graph;
use crate::store::{Capabilities, GraphStore, HolderDeriveSource};
use crate::types::{tie_break_by_key, EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// Where a caller's vector candidates come from (#27, caller side).
///
/// Recall and hybrid derive are given one of these instead of a store, and ask
/// it two things: whether a vector leg exists at all ([`Self::available`], no
/// I/O), and the checked candidates for a probe ([`Self::checked`]).
///
/// Three sources:
///
/// * [`Self::Store`] — the durable store's checked read, with exactly the
///   behaviour callers had when they called it directly: the capability bit is
///   `Capabilities::VECTOR_SEARCH`, and the read is
///   `GraphStore::vector_candidates_checked`, capability refusal included.
///   Readers that hold no live graph (`lambo recall`, `serve-web`) always use
///   it, and so does a holder whose store ranks in the database.
/// * [`Self::Graph`] — the session holder's in-memory graph (#8), chosen by
///   [`Self::for_holder`] only when the store's own checked read is the same
///   exact scan (`GraphStore::exact_vector_scan`), so the answer is the same
///   and the store is not touched.
/// * [`Self::StoreAndUnflushed`] — the store for what the flush made durable
///   plus the holder's unflushed concepts (#60), chosen only by
///   [`Self::for_holder_derive`] over a store that declares it
///   (`GraphStore::holder_derive_source`).
///
/// An enum rather than a trait object so both paths stay statically
/// dispatched. Another candidate source (#18's Elastic tier) arrives as a
/// store: it implements `vector_candidates_checked` through its own
/// [`VectorCandidateSource`] and leaves `exact_vector_scan` false.
#[derive(Clone, Copy)]
pub(crate) enum VectorCandidates<'a> {
    /// The durable store's checked read.
    Store(&'a dyn GraphStore),
    /// The session holder's graph, ranked in place (#8).
    Graph(GraphVectorSource<'a>),
    /// The store for what the flush made durable, plus the holder's
    /// unflushed concepts ranked in place (#60). Only hybrid derive on a
    /// holder over the Postgres family is handed this.
    StoreAndUnflushed(StoreAndUnflushedSource<'a>),
}

impl<'a> VectorCandidates<'a> {
    /// The store as the source.
    pub(crate) fn from_store(store: &'a dyn GraphStore) -> Self {
        Self::Store(store)
    }

    /// The source a **session holder** hands its recall and hybrid derive
    /// (#8). The one place the choice is made; `Memory::vector_candidates` and
    /// `WriteCtx::vector_candidates` both call it, so the synchronous and the
    /// background write paths cannot disagree.
    ///
    /// The holder's graph is chosen when the store both advertises
    /// `VECTOR_SEARCH` and declares its checked read an exact scan of the
    /// vectors it holds (`GraphStore::exact_vector_scan`). The graph holds
    /// those same vectors (and the unflushed ones besides), so ranking them
    /// in RAM gives the store's answer without its I/O. Otherwise the store
    /// stays the source: a store without `VECTOR_SEARCH` keeps the vector leg
    /// off exactly as before (the graph source is never offered as a way to
    /// switch it on), and a store that ranks in the database keeps doing so.
    ///
    /// Synchronous and lock-free: it reads the store's two declarations and
    /// never touches the graph, so a caller may hold the graph lock.
    pub(crate) fn for_holder(store: &'a dyn GraphStore, graph: &'a RwLock<Graph>) -> Self {
        if store.capabilities().contains(Capabilities::VECTOR_SEARCH) && store.exact_vector_scan() {
            Self::Graph(GraphVectorSource::new(graph))
        } else {
            Self::Store(store)
        }
    }

    /// The source a **session holder**'s hybrid derive is handed (#18,
    /// amending #8's "one constructor, two callers"; #60).
    ///
    /// The holder's graph whenever [`Self::for_holder`] would choose it.
    /// Otherwise the store's declaration decides
    /// ([`GraphStore::holder_derive_source`]): a store that lags the holder
    /// beyond its flush (the Elastic tier, #18) gets the whole graph; one
    /// whose search sees each flush commit (the Postgres family, #60) gets
    /// the store plus the holder's unflushed concepts. Derive's dedupe needs
    /// a fresh view of what was just written, which a lagging store alone
    /// does not have. Recall still takes [`Self::for_holder`]'s choice. Same
    /// availability as [`Self::for_holder`]: never switches a vector leg on.
    pub(crate) fn for_holder_derive(store: &'a dyn GraphStore, graph: &'a RwLock<Graph>) -> Self {
        if !store.capabilities().contains(Capabilities::VECTOR_SEARCH) {
            return Self::Store(store);
        }
        if store.exact_vector_scan() {
            return Self::Graph(GraphVectorSource::new(graph));
        }
        match store.holder_derive_source() {
            HolderDeriveSource::Graph => Self::Graph(GraphVectorSource::new(graph)),
            HolderDeriveSource::StoreAndUnflushed => {
                Self::StoreAndUnflushed(StoreAndUnflushedSource::new(store, graph))
            }
            _ => Self::Store(store),
        }
    }

    /// Whether the vector leg can run at all. Synchronous and I/O-free, so a
    /// caller can skip the query embed when it cannot.
    pub(crate) fn available(&self) -> bool {
        match self {
            Self::Store(store) => store.capabilities().contains(Capabilities::VECTOR_SEARCH),
            // Chosen only over a store that advertises VECTOR_SEARCH.
            Self::Graph(_) => true,
            Self::StoreAndUnflushed(union) => union
                .store()
                .capabilities()
                .contains(Capabilities::VECTOR_SEARCH),
        }
    }

    /// Whether a checked read can come back as a capability refusal, the
    /// signal hybrid derive degrades on. Only the store can refuse: the
    /// holder's graph always answers, so asking it just to learn that costs a
    /// whole scan for nothing (#60 review L3). Synchronous and I/O-free.
    pub(crate) fn can_refuse(&self) -> bool {
        !matches!(self, Self::Graph(_))
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
            Self::Graph(graph) => {
                graph
                    .checked_vector_candidates(session, probe, expected_contract, limit)
                    .await
            }
            Self::StoreAndUnflushed(union) => {
                union
                    .checked_vector_candidates(session, probe, expected_contract, limit)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CanonizationEvent, GraphSnapshot, InteractionSpan, MutationBatch, StoreError,
    };

    /// Only the two declarations `for_holder` reads matter; every async
    /// surface panics, so the selection is proven I/O-free as well.
    struct Declares {
        caps: Capabilities,
        exact: bool,
        derive: HolderDeriveSource,
    }

    #[async_trait]
    impl GraphStore for Declares {
        async fn init_schema(&self) -> Result<(), StoreError> {
            unreachable!("selection is I/O-free")
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn exact_vector_scan(&self) -> bool {
            self.exact
        }
        fn holder_derive_source(&self) -> HolderDeriveSource {
            self.derive
        }
        async fn flush(&self, _: &MutationBatch, _: Option<u64>) -> Result<(), StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn load_session(&self, _: &SessionId) -> Result<GraphSnapshot, StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn keyword_candidates(
            &self,
            _: &SessionId,
            _: &[String],
            _: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn vector_candidates(
            &self,
            _: &SessionId,
            _: &[f32],
            _: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn blast_radius(
            &self,
            _: &SessionId,
            _: NodeId,
            _: std::time::Duration,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn interaction_span(
            &self,
            _: &SessionId,
            _: NodeId,
            _: std::time::Duration,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<InteractionSpan, StoreError> {
            unreachable!("selection is I/O-free")
        }
        async fn record_canonization(
            &self,
            _: &CanonizationEvent,
            _: Option<u64>,
        ) -> Result<(), StoreError> {
            unreachable!("selection is I/O-free")
        }
    }

    /// #8: the holder's graph is chosen only over a vector-capable store that
    /// declares an exact scan; everything else keeps the store, including a
    /// store without `VECTOR_SEARCH`, whose vector leg must stay off.
    #[test]
    fn for_holder_picks_the_graph_only_for_an_exact_vector_store() {
        let graph = RwLock::new(Graph::new(SessionId::from("s")));
        for (caps, exact, want_graph, want_available) in [
            (Capabilities::VECTOR_SEARCH, true, true, true),
            (Capabilities::VECTOR_SEARCH, false, false, true),
            (Capabilities::HISTORY, true, false, false),
            (Capabilities::HISTORY, false, false, false),
            (Capabilities::empty(), true, false, false),
        ] {
            let store = Declares {
                caps,
                exact,
                derive: HolderDeriveSource::Store,
            };
            let source = VectorCandidates::for_holder(&store, &graph);
            assert_eq!(
                matches!(source, VectorCandidates::Graph(_)),
                want_graph,
                "caps {caps:?} exact {exact}"
            );
            assert_eq!(
                source.available(),
                want_available,
                "caps {caps:?} exact {exact}"
            );
        }
    }

    /// The trait default keeps every adapter that does not opt in on the store.
    #[test]
    fn exact_vector_scan_defaults_to_false() {
        struct Plain(Declares);
        #[async_trait]
        impl GraphStore for Plain {
            async fn init_schema(&self) -> Result<(), StoreError> {
                self.0.init_schema().await
            }
            fn capabilities(&self) -> Capabilities {
                self.0.capabilities()
            }
            async fn flush(&self, b: &MutationBatch, t: Option<u64>) -> Result<(), StoreError> {
                self.0.flush(b, t).await
            }
            async fn load_session(&self, s: &SessionId) -> Result<GraphSnapshot, StoreError> {
                self.0.load_session(s).await
            }
            async fn keyword_candidates(
                &self,
                s: &SessionId,
                t: &[String],
                l: usize,
            ) -> Result<Vec<Scored<NodeId>>, StoreError> {
                self.0.keyword_candidates(s, t, l).await
            }
            async fn vector_candidates(
                &self,
                s: &SessionId,
                e: &[f32],
                l: usize,
            ) -> Result<Vec<Scored<NodeId>>, StoreError> {
                self.0.vector_candidates(s, e, l).await
            }
            async fn blast_radius(
                &self,
                s: &SessionId,
                n: NodeId,
                a: std::time::Duration,
                now: chrono::DateTime<chrono::Utc>,
            ) -> Result<u64, StoreError> {
                self.0.blast_radius(s, n, a, now).await
            }
            async fn interaction_span(
                &self,
                s: &SessionId,
                n: NodeId,
                a: std::time::Duration,
                now: chrono::DateTime<chrono::Utc>,
            ) -> Result<InteractionSpan, StoreError> {
                self.0.interaction_span(s, n, a, now).await
            }
            async fn record_canonization(
                &self,
                e: &CanonizationEvent,
                t: Option<u64>,
            ) -> Result<(), StoreError> {
                self.0.record_canonization(e, t).await
            }
        }
        let plain = Plain(Declares {
            caps: Capabilities::VECTOR_SEARCH,
            exact: true,
            derive: HolderDeriveSource::Graph,
        });
        assert!(!plain.exact_vector_scan(), "a wrapper must opt in itself");
        assert_eq!(
            plain.holder_derive_source(),
            HolderDeriveSource::Store,
            "a wrapper must opt in itself"
        );
        let graph = RwLock::new(Graph::new(SessionId::from("s")));
        assert!(matches!(
            VectorCandidates::for_holder(&plain, &graph),
            VectorCandidates::Store(_)
        ));
        assert!(matches!(
            VectorCandidates::for_holder_derive(&plain, &graph),
            VectorCandidates::Store(_)
        ));
    }

    /// #18 amending #8, and #60: derive takes the graph wherever recall
    /// does; otherwise the store's `holder_derive_source` picks the whole
    /// graph (a lagging tier) or the store plus the unflushed concepts (the
    /// Postgres family). Recall's choice is unchanged by that declaration,
    /// and neither ever switches a vector leg on.
    #[test]
    fn for_holder_derive_follows_the_store_declaration() {
        use HolderDeriveSource as D;
        let graph = RwLock::new(Graph::new(SessionId::from("s")));
        let vs = Capabilities::VECTOR_SEARCH;
        // (caps, exact, declared, recall on graph, derive source, available)
        for (caps, exact, declared, recall_graph, want, available) in [
            (vs, true, D::Store, true, "graph", true),
            (vs, false, D::Store, false, "store", true),
            (vs, false, D::Graph, false, "graph", true),
            (vs, false, D::StoreAndUnflushed, false, "union", true),
            (vs, true, D::Graph, true, "graph", true),
            (vs, true, D::StoreAndUnflushed, true, "graph", true),
            (
                Capabilities::HISTORY,
                false,
                D::Graph,
                false,
                "store",
                false,
            ),
            (
                Capabilities::HISTORY,
                false,
                D::StoreAndUnflushed,
                false,
                "store",
                false,
            ),
            (Capabilities::empty(), true, D::Graph, false, "store", false),
        ] {
            let store = Declares {
                caps,
                exact,
                derive: declared,
            };
            let recall = VectorCandidates::for_holder(&store, &graph);
            let derive = VectorCandidates::for_holder_derive(&store, &graph);
            let case = format!("caps {caps:?} exact {exact} declared {declared:?}");
            assert_eq!(
                matches!(recall, VectorCandidates::Graph(_)),
                recall_graph,
                "{case}"
            );
            let got = match derive {
                VectorCandidates::Store(_) => "store",
                VectorCandidates::Graph(_) => "graph",
                VectorCandidates::StoreAndUnflushed(_) => "union",
            };
            assert_eq!(got, want, "{case}");
            assert_eq!(derive.available(), available, "{case}");
            assert_eq!(recall.available(), available, "{case}");
        }
    }
}
