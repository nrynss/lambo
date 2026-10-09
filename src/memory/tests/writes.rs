//! Write paths: index mirroring, retract, hybrid derive, synonyms and
//! reservations.

use super::*;

/// `MemoryStore` plus a **real** vector leg: exact cosine over the
/// embeddings that actually survived the write-behind flush. It is the
/// in-process stand-in for Cockroach's `concepts_embedding_idx` (the only
/// adapter that advertises `VECTOR_SEARCH`), which is what lets a default
/// `cargo test` assert L82-4 end to end — derive → flush → vector recall on
/// organically-derived data — with no live cluster. Every answer it gives
/// is recorded so a test can prove the vector leg *fired* rather than
/// inferring it from a rank.
///
/// Built with [`VectorSearchStore::graph_ranked`] it also declares its
/// checked read an exact scan, so a holder ranks in its own graph instead
/// (#8's `VectorCandidates::Graph`); the #14 query-cache tests run on both.
pub(super) struct VectorSearchStore {
    inner: Arc<dyn GraphStore>,
    answers: PlMutex<Vec<Vec<Scored<NodeId>>>>,
    exact_scan: bool,
    /// When set, the checked read fails as a backend would (#22 PR 6, M1).
    fail_reads: std::sync::atomic::AtomicBool,
}

impl VectorSearchStore {
    pub(super) fn new(inner: Arc<dyn GraphStore>) -> Self {
        Self {
            inner,
            answers: PlMutex::new(Vec::new()),
            exact_scan: false,
            fail_reads: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The same store, declaring `exact_vector_scan`, so a holder ranks its
    /// graph's vectors and never calls the checked read.
    pub(super) fn graph_ranked(inner: Arc<dyn GraphStore>) -> Self {
        Self {
            exact_scan: true,
            ..Self::new(inner)
        }
    }

    /// Make every later checked read fail with a `StoreError::Backend`, as
    /// a timed-out or unreachable backend would.
    pub(super) fn fail_vector_reads(&self) {
        self.fail_reads
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Every `vector_candidates` answer, in call order.
    pub(super) fn answers(&self) -> Vec<Vec<Scored<NodeId>>> {
        self.answers.lock().clone()
    }
}

#[async_trait]
impl GraphStore for VectorSearchStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities() | Capabilities::VECTOR_SEARCH
    }
    fn vector_dimensions(&self) -> Option<usize> {
        Some(1024)
    }
    fn exact_vector_scan(&self) -> bool {
        self.exact_scan
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.inner.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        panic!("VectorSearchStore: unchecked vector lookup")
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        crate::store::validate_vector_candidate_limit(limit)?;
        if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Backend(
                "vector read failed: backend db.internal:5432 timed out".into(),
            ));
        }
        // A session with nothing flushed yet is an EMPTY candidate pool, not
        // an error — the shape Cockroach returns for an unstamped session
        // before the first commit.
        let snapshot = match self.inner.load_session(session).await {
            Ok(snapshot) => snapshot,
            Err(StoreError::SessionNotFound(_)) => {
                self.answers.lock().push(Vec::new());
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        // H1/E2E-1: like Cockroach's checked read, bind the query's
        // expected contract to the DURABLE contract and refuse a change —
        // never silently answer with vectors the caller cannot interpret.
        // (Legacy unstamped vectors are quarantined at load, so an
        // unrecorded durable contract is an empty pool.)
        match &snapshot.embedding {
            None => {
                self.answers.lock().push(Vec::new());
                return Ok(Vec::new());
            }
            Some(durable) if durable == expected_contract => {}
            Some(durable) => {
                return Err(StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: \
                         vectors were written by kind={} model={:?} dim={}, but the live/attached \
                         embedder is kind={} model={:?} dim={} — re-embed or start a new session",
                    durable.kind,
                    durable.model,
                    durable.dim,
                    expected_contract.kind,
                    expected_contract.model,
                    expected_contract.dim,
                )));
            }
        }
        // Same ordering contract as the real adapters: best first, ties
        // broken by canonical key asc then the smaller UUID (issue #2) so
        // the answer is deterministic within and across runs.
        let mut scored: Vec<(Scored<NodeId>, &str)> = snapshot
            .concepts
            .iter()
            .filter_map(|c| {
                let vector = c.embedding.as_ref()?;
                Some((
                    Scored::new(c.id, f64::from(crate::embed::cosine(embedding, vector))),
                    c.canonical_key.as_str(),
                ))
            })
            .collect();
        scored.sort_by(|(a, a_key), (b, b_key)| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
        });
        let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
        scored.truncate(limit);
        self.answers.lock().push(scored.clone());
        Ok(scored)
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.inner.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.inner.release_lease(session, holder).await
    }
}

/// `FixtureEmbedder` is hash-seeded per exact phrase, which cannot model the
/// one thing an organic vector-recall test needs: hybrid embeds a concept
/// **with** its origin context (`"register user — <prompt>"`, the PHASE-7
/// calibration rule) while recall embeds the **bare** query
/// (`"create account"`), and a real semantic embedder still scores those two
/// as near. This wrapper reduces the context framing back to the concept
/// label before delegating — behaviour BGE-M3 has for free and a hash
/// fixture cannot. Nothing else about the embedding path is altered.
#[derive(Debug)]
pub(super) struct ContextTolerantEmbedder(pub(super) FixtureEmbedder);

#[async_trait]
impl Embedder for ContextTolerantEmbedder {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.0.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

impl ContextTolerantEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let label = text
            .strip_prefix("Concept: ")
            .unwrap_or(text)
            .split(" — ")
            .next()
            .unwrap_or(text);
        role.embed(&self.0, label).await
    }
}

// -- index mirroring ----------------------------------------------------

/// The `src/graph/mod.rs` contract, at the `Memory` level: every concept
/// create — `derive`, `record_action` AND `demote` — must be searchable,
/// and a retraction must stop being searchable.
#[tokio::test]
async fn every_write_path_mirrors_the_inverted_index() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "mirror").await;

    let out = mem
        .derive(
            &[
                ("pagination", ConceptType::Entity),
                ("rate limiter", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    assert_eq!(out.created.len(), 2);
    assert!(!mem.index().read().search("pagination", 10).is_empty());
    assert!(!mem.index().read().search("limiter", 10).is_empty());

    mem.record_action(&Action {
        event_time: None,
        action: "wrote docs/api.md",
        produces: &["docs/api.md"],
        modifies: &[],
        depends_on: &["pagination"],
    })
    .unwrap();
    assert!(
        !mem.index().read().search("docs/api.md", 10).is_empty(),
        "record_action creations must be mirrored"
    );

    let obs = mem
        .demote("The caching layer was the bottleneck.", "chunk-9")
        .unwrap();
    assert_eq!(obs.len(), 1);
    assert!(
        !mem.index().read().search("caching", 10).is_empty(),
        "demote creations must be mirrored"
    );

    // Retraction removes the posting too.
    mem.retract("rate limiter", DryRun::No).await.unwrap();
    assert!(
        mem.index().read().search("limiter", 10).is_empty(),
        "stale posting after retract"
    );

    mem.close().await.unwrap();
}

// -- retract ------------------------------------------------------------

#[tokio::test]
async fn retract_dry_run_reports_impact_and_mutates_nothing() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "retract-dry").await;

    mem.derive(
        &[("stale dependency", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    mem.record_action(&Action {
        event_time: None,
        action: "wired the client",
        produces: &[],
        modifies: &[],
        depends_on: &["stale dependency"],
    })
    .unwrap();

    let before_nodes = mem.stats().node_count;
    let before_edges = mem.stats().edge_count;
    let before_epoch = mem.stats().epoch;

    let report = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
    assert!(report.dry_run);
    assert!(!report.removed);
    assert_eq!(report.content, "stale dependency");
    assert!(report.incident_edges > 0);

    let after = mem.stats();
    assert_eq!(after.node_count, before_nodes, "dry run must not remove");
    assert_eq!(after.edge_count, before_edges);
    assert_eq!(after.epoch, before_epoch, "dry run must not log a mutation");
    assert!(
        !mem.index().read().search("stale", 10).is_empty(),
        "dry run must not touch the index"
    );

    mem.close().await.unwrap();
}

#[tokio::test]
async fn retract_removes_the_node_and_its_edges() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "retract-live").await;

    mem.derive(
        &[
            ("stale dependency", ConceptType::Entity),
            ("http client", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let before = mem.stats();

    let report = mem.retract("stale dependency", DryRun::No).await.unwrap();
    assert!(report.removed);
    assert!(!report.dry_run);

    let after = mem.stats();
    assert_eq!(after.node_count, before.node_count - 1);
    assert!(after.edge_count < before.edge_count);
    assert!(mem.graph().read().node(report.target).is_none());

    mem.close().await.unwrap();
}

/// R2-5: `retract`'s durable-radius query was the only store call on a
/// user-facing path with no bound, and the writers gate made it `close()`'s
/// problem — `retract` holds its permit across that await, so step 0 waited
/// on the hung backend for as long as it cared to hang.
///
/// Bounded, it fails at `RETRACT_IO_TIMEOUT` having mutated nothing (the
/// await precedes every graph write), and the session still closes.
#[tokio::test(start_paused = true)]
async fn retract_bounds_a_hanging_durable_radius_query() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    // Parked and never released: the backend that never answers.
    let parking = Arc::new(ParkingStore::new(inner, ParkPoint::BlastRadius));
    let store: Arc<dyn GraphStore> = parking.clone();
    let mem = memory_on(store, "retract-hang").await;

    mem.derive(
        &[("stale dependency", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let before = mem.stats();

    // The outer bound is the test's safety net, an order of magnitude past
    // the real one: an unbounded `retract` fails this assertion instead of
    // wedging the suite on a store that never answers.
    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        RETRACT_IO_TIMEOUT * 10,
        mem.retract("stale dependency", DryRun::No),
    )
    .await
    .expect("retract must bound its own durable-radius query")
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        started.elapsed() >= RETRACT_IO_TIMEOUT,
        "the bound must be the timeout, not something shorter"
    );

    // Nothing half-done: the removal is below the await, so a refused
    // retraction leaves the node, its edges and its index posting.
    let after = mem.stats();
    assert_eq!(after.node_count, before.node_count);
    assert_eq!(after.edge_count, before.edge_count);
    assert_eq!(after.epoch, before.epoch, "no mutation was logged");
    assert!(!mem.index().read().search("stale", 10).is_empty());

    // ...and the writers gate is free again, so shutdown is bounded too.
    tokio::time::timeout(Duration::from_secs(60), mem.close())
        .await
        .expect("close() must not wait on the hung query")
        .expect("close");
}

/// R3-3: the bound applies to [`DryRun::Yes`] as well, and the rustdoc now
/// says so.
///
/// A dry run mutates nothing, so it *could* have degraded to the warning
/// path the store-error arm uses — the reason it does not is that the
/// asymmetry is a judgement about the store, not about this call, and a dry
/// run is the preview the real retraction gets authorised from. Whichever
/// way that decision goes it must be pinned, because the two arms of the
/// same `match` now differ on it.
#[tokio::test(start_paused = true)]
async fn retract_bounds_a_hanging_query_for_a_dry_run_too() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let parking = Arc::new(ParkingStore::new(inner, ParkPoint::BlastRadius));
    let store: Arc<dyn GraphStore> = parking.clone();
    let mem = memory_on(store, "retract-hang-dry").await;

    mem.derive(
        &[("stale dependency", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    let before = mem.stats();

    let err = tokio::time::timeout(
        RETRACT_IO_TIMEOUT * 10,
        mem.retract("stale dependency", DryRun::Yes),
    )
    .await
    .expect("a dry run must bound its durable-radius query too")
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");

    // Inert as ever — the failure changes nothing about that.
    let after = mem.stats();
    assert_eq!(after.node_count, before.node_count);
    assert_eq!(after.edge_count, before.edge_count);
    assert_eq!(after.epoch, before.epoch, "no mutation was logged");

    tokio::time::timeout(Duration::from_secs(60), mem.close())
        .await
        .expect("close() must not wait on the hung query")
        .expect("close");
}

#[tokio::test]
async fn retract_reports_an_unknown_target_as_not_found() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "retract-missing").await;
    let err = mem.retract("nothing here", DryRun::Yes).await.unwrap_err();
    assert!(err.to_string().contains("no concept matching"), "{err}");
    mem.close().await.unwrap();
}

/// The store answers once the session is flushed; before that the report
/// degrades to the in-RAM radius with a warning instead of failing.
#[tokio::test]
async fn retract_reports_the_durable_radius_when_the_store_can_answer() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store.clone(), "retract-durable").await;

    mem.derive(
        &[("stale dependency", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .unwrap();

    let unflushed = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
    assert!(unflushed.durable_blast_radius.is_none());
    assert_eq!(unflushed.warnings.len(), 1);

    mem.close().await.unwrap();

    // Re-attach over the now-durable session and ask again.
    let mem = memory_on(store, "retract-durable").await;
    let flushed = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
    assert_eq!(flushed.durable_blast_radius, Some(flushed.blast_radius));
    assert!(flushed.warnings.is_empty());
    mem.close().await.unwrap();
}

/// `MatchStrategy::Hybrid` routes `derive` through the async
/// `graph::hybrid::derive` twin. Against a store without `VECTOR_SEARCH`
/// (MemoryStore) the hybrid step is skipped and the outcome is identical to
/// the canonical path — what matters here is that the dispatch compiles,
/// runs, holds no lock across its awaits, and still mirrors the index.
#[tokio::test]
async fn hybrid_strategy_derives_and_mirrors_the_index() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = Memory::builder()
        .session("hybrid")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();

    let out = mem
        .derive(
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    assert_eq!(out.created.len(), 2);
    assert!(!mem.index().read().search("schema", 10).is_empty());

    // A second derive of the same content matches rather than duplicating.
    let again = mem
        .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert!(again.created.is_empty());
    assert_eq!(again.matched.len(), 1);

    mem.close().await.unwrap();
    let snap = store.load_session(&SessionId::new("hybrid")).await.unwrap();
    assert_eq!(snap.concepts.len(), 2);
}

/// **L82-4 end to end.** A concept created by the ordinary derive surface
/// persists its embedding, the vector survives the write-behind flush and a
/// session reload, and recall's vector leg finds it. This is the path that
/// stored `embedding IS NULL` for all 13 organic concepts the live-Cockroach
/// review measured — recall was keyword/recency only on everything the
/// product itself wrote (`adve-review-t8.2-t8.3-live.md`, L82-4).
#[tokio::test]
async fn organic_derive_persists_a_vector_that_recall_finds() {
    use crate::embed::{NEAR_A, NEAR_B};

    let store = Arc::new(VectorSearchStore::new(
        Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>
    ));
    let session = SessionId::new("organic-vectors");
    let open = |store: Arc<VectorSearchStore>| async move {
        Memory::builder()
            .session("organic-vectors")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .match_strategy(MatchStrategy::Hybrid)
            .store(store as Arc<dyn GraphStore>)
            .embedder(
                Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>
            )
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("build")
    };

    let mem = open(store.clone()).await;
    let out = mem
        .derive(&[(NEAR_A, ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert_eq!(out.created.len(), 1);
    let organic = out.created[0];
    assert!(
        out.semantic_merged.is_empty(),
        "empty vector pool — nothing to merge with, so the concept is Fresh"
    );
    // close() drains the tail: the vector has to be durable, not RAM-only.
    mem.close().await.unwrap();

    // The reviewer's `SELECT embedding IS NOT NULL`, in process.
    let snapshot = store.load_session(&session).await.unwrap();
    let stored = snapshot
        .concepts
        .iter()
        .find(|c| c.id == organic)
        .expect("the derived concept is durable");
    assert_eq!(
        stored.embedding.as_ref().map(Vec::len),
        Some(1024),
        "an organically-derived concept persists its vector (L82-4)"
    );
    assert_eq!(
        store.answers(),
        vec![Vec::new()],
        "the derive's own gather queried an empty pool and merged nothing"
    );

    // Reopen (proving the vector round-trips through load_session) and
    // recall with text that shares NO token with the stored concept
    // ("create account" vs "register user") but is near it in the embedding
    // space — the keyword leg cannot score it, only the vector leg can.
    let reopened = open(store.clone()).await;
    let result = reopened
        .recall(RecallQuery {
            query: NEAR_B.into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 1,
        })
        .await
        .unwrap();

    let answers = store.answers();
    assert_eq!(answers.len(), 2, "recall issued exactly one vector query");
    let scored = answers[1]
        .iter()
        .find(|s| s.item == organic)
        .expect("the vector leg returned the organically-derived concept");
    assert!(
        scored.score >= 0.85,
        "organic vector scored by real similarity, got {}",
        scored.score
    );
    assert!(
        result.hits.iter().any(|h| h.node_id == organic),
        "the vector-leg candidate reaches the assembled result: {result:?}"
    );
    assert!(
        !result
            .warnings
            .iter()
            .any(|w| w.contains("vector leg skipped")),
        "the vector leg must not degrade here: {:?}",
        result.warnings
    );

    reopened.close().await.unwrap();
}

/// **E2E-1 regression.** The `--allow-embedding-mismatch` relabel must be
/// durable BEFORE the first write on a `VECTOR_SEARCH`-capable store: the
/// checked candidate read compares the *durable* contract against the
/// expected one, so a write-behind relabel (flush at interval / close)
/// made the documented override workflow refuse its FIRST hybrid write
/// with `Invariant` (live-reproduced on Cockroach; the identical
/// invocation only succeeded on the second run, once `close()` had landed
/// the relabel). This is the default-features guard: MemoryStore does not
/// advertise `VECTOR_SEARCH`, so its own override test never reaches the
/// hybrid checked read, while [`VectorSearchStore`] does advertise it and
/// enforces the contract like Cockroach. (SQLite reaches it for real since
/// F2, but only under the `store-sqlite` feature row.)
#[tokio::test]
async fn h1_override_relabel_is_durable_before_the_first_hybrid_write() {
    let store = Arc::new(VectorSearchStore::new(
        Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>
    ));
    let old = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-v1".into()),
        dim: 1024,
    };
    let live = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-renamed".into()),
        dim: 1024,
    };
    let open = |store: Arc<VectorSearchStore>, contract: EmbeddingContract, allow: bool| {
        let store = store.clone();
        async move {
            Memory::builder()
                .session("e2e1-override-first-write")
                .agent("operator")
                .flush_interval(Duration::from_secs(3_600))
                .match_strategy(MatchStrategy::Hybrid)
                .store(store as Arc<dyn GraphStore>)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(contract)
                .allow_embedding_mismatch(allow)
                .build()
                .await
                .expect("attach")
        }
    };

    // Writer 1: contract A; the derive writes a real vector through the
    // checked read (empty pool on the fresh, unstamped session).
    let first = open(store.clone(), old.clone(), false).await;
    first
        .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    first.close().await.unwrap();

    // Writer 2: same kind, same width, renamed model, explicit override.
    // The FIRST write must now succeed — the relabel was flushed to the
    // store at attach, so the checked read sees the new durable contract.
    let second = open(store.clone(), live.clone(), true).await;
    second
        .derive(
            &[("auth middleware", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!("override relabel must be durable before the first hybrid write: {err}")
        });
    second.close().await.unwrap();

    // The durable contract is the relabeled one, not the original.
    let snapshot = store
        .load_session(&SessionId::new("e2e1-override-first-write"))
        .await
        .unwrap();
    assert_eq!(snapshot.embedding, Some(live));
}

#[tokio::test]
async fn declared_synonyms_merge_on_derive() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "synonyms").await;

    mem.declare_synonym("register_user", "create_user").unwrap();
    let first = mem
        .derive(&[("create_user", ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    let second = mem
        .derive(&[("register_user", ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();

    assert_eq!(first.created.len(), 1);
    assert!(
        second.created.is_empty(),
        "the synonym must match, not create"
    );
    assert_eq!(second.matched, first.created);

    mem.close().await.unwrap();
}

#[tokio::test]
async fn reserve_and_release_round_trip() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "reserve").await;

    let out = mem
        .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let node = out.created[0];

    let reservation = mem.reserve(node, Duration::from_secs(30)).unwrap();
    assert_eq!(reservation.node_id, node);
    assert_eq!(reservation.agent_id, AgentId::new("agent-a"));
    assert!(mem.graph().read().reservation(node).is_some());

    mem.release(node).unwrap();
    assert!(mem.graph().read().reservation(node).is_none());

    mem.close().await.unwrap();
}

#[tokio::test]
async fn root_goal_promotes_matching_concepts_to_venerable() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "goal").await;

    mem.derive(&[("3D renderer", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    mem.set_root_goal(&["doom-style FPS", "3D renderer"])
        .unwrap();

    let status = {
        let g = mem.graph().read();
        let found = g
            .concepts()
            .find(|c| c.content == "3D renderer")
            .map(|c| c.canonization_status);
        found.unwrap()
    };
    assert_eq!(status, CanonizationStatus::Venerable);

    mem.close().await.unwrap();
}

/// Hybrid `derive` embeds nothing when the store has no `VECTOR_SEARCH`: a
/// store that cannot search vectors keeps none. `record_action` follows the
/// same rule, so one keyword-only session does not report "(0 embedded)" for
/// a derive and "(N embedded)" for an action.
#[tokio::test]
async fn a_hybrid_record_action_on_a_store_without_vector_search_embeds_nothing() {
    let mem = Memory::builder()
        .session("action-no-vector")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    let stamp_before = mem.graph.read().embedding().cloned();
    let out = mem
        .record_action_embedded_as(
            &AgentId::new("agent-a"),
            &Action {
                event_time: None,
                action: "ran the migration",
                produces: &["schema v2"],
                modifies: &[],
                depends_on: &[],
            },
        )
        .await
        .expect("record_action");
    assert_eq!(out.created.len(), 2);
    assert_eq!(out.embedded, 0);
    let g = mem.graph.read();
    assert!(g.concepts().all(|c| c.embedding.is_none()));
    assert_eq!(
        g.embedding().cloned(),
        stamp_before,
        "the stamp is untouched"
    );
}

// -- the write queue's probe embeds what a derive embeds (#11) -----------

/// An embedder that records every text it is asked to embed.
#[derive(Debug)]
struct RecordingEmbedder {
    inner: FixtureEmbedder,
    texts: parking_lot::Mutex<Vec<String>>,
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl RecordingEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.texts.lock().push(text.to_string());
        role.embed(&self.inner, text).await
    }
}

/// **The calibration probe embeds exactly what a real derive of its
/// concepts embeds** (#11 review P3-7).
///
/// The probe's representative write exists so `probe_optimism` compares a
/// write with a write. The test beside the probe compared the probe's texts
/// with the framing helpers the probe itself calls, which proves the probe
/// uses the helpers and not that a derive embeds that. This one runs the
/// probe's own concepts through a real hybrid derive, through the write
/// queue, and compares what the embedder was asked to embed.
#[tokio::test]
async fn a_real_derive_of_the_probes_concepts_embeds_the_probes_texts() {
    let inner = Arc::new(MemoryStore::new());
    let embedder = Arc::new(RecordingEmbedder {
        inner: FixtureEmbedder::new(),
        texts: parking_lot::Mutex::new(Vec::new()),
    });
    let agent = AgentId::new("agent-a");
    let mem = Memory::builder()
        .session("probe-is-a-derive")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(super::replay::VectorSearchable(inner)) as Arc<dyn GraphStore>)
        .embedder(embedder.clone() as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    // The probe embeds through the same embedder; let it finish first.
    for _ in 0..2_000 {
        if mem.pipeline().calibration().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(mem.pipeline().calibration().is_some(), "the probe landed");
    embedder.texts.lock().clear();

    let concepts = crate::writeq::probe_write_concepts();
    let typed: Vec<(&str, ConceptType)> = concepts
        .iter()
        .map(|c| (c.as_str(), ConceptType::Logic))
        .collect();
    let submitted = mem
        .derive_async_as(&agent, &typed, &ParentOf::none(), None)
        .await
        .expect("ack");
    let answer = mem
        .pipeline()
        .wait(&agent, submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(answer.tag(), "applied", "{answer:?}");

    let embedded = embedder.texts.lock().clone();
    assert_eq!(
        embedded,
        crate::writeq::probe_write_contexts(),
        "the probe's representative write must embed what a derive of its concepts embeds"
    );
    mem.close().await.expect("close");
}
