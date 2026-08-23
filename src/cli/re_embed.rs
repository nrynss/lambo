//! `lambo re-embed` — the K2 migration verb: rewrite a session's EVERY concept
//! vector into the configured live embedding space and swap the session's
//! [`EmbeddingContract`] in the same atomic flush.
//!
//! Why this exists: Level B refuses to attach a writer whose stored contract
//! disagrees with the configured embedder, because two model spaces in one
//! session is the corruption it guards against. That refusal is correct for
//! every ordinary attach — and exactly wrong for the one operation that ENDS a
//! mixed-space risk instead of creating one. `re-embed` attaches through
//! [`crate::memory::MemoryBuilder::reembed_mode`], embeds every concept
//! content outside any lock, then appends the whole rewrite as one ordered
//! batch via [`crate::graph::Graph::reembed_all`]: every `UpsertNode` first,
//! the trailing `SetEmbedding` last. The final flush in
//! [`super::close_writer`] drains that batch transactionally with the lease
//! token (one store transaction per batch), so the durable session goes
//! old-consistent → new-consistent with no observable window in between —
//! and on a crash mid-migration the old contract and old vectors survive
//! together, which is consistent.
//!
//! This is the migration path; it deliberately REFUSES
//! `--allow-embedding-mismatch`, which relabels vectors without rewriting them.

use std::sync::Arc;

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::{close_writer, map_writer_err};
use crate::embed::Embedder;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::types::{EmbeddingContract, NodeId};

/// Parsed `re-embed` flags.
pub struct Args {
    pub session: String,
    pub agent: String,
    /// Accepted on the wire only so the clap shape mirrors the other writers;
    /// always refused. Re-embed IS the mismatch migration.
    pub allow_embedding_mismatch: bool,
}

/// Run the K2 re-embed migration for one session.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    if args.allow_embedding_mismatch {
        return Err(CliError::Usage(
            "--allow-embedding-mismatch is refused by re-embed: re-embed IS the embedding \
             migration. It attaches past the contract check itself, embeds every concept into \
             the configured live space, and swaps the contract atomically; pairing it with the \
             relabel override (which leaves old-space vectors mislabelled) would be \
             contradictory. Drop the flag."
                .to_string(),
        ));
    }

    // The builder is constructed directly because `open_writer` does not expose
    // `reembed_mode`. Backends pass through UNCHANGED:
    // `allow_embedding_mismatch` stays false (the override field is simply
    // unused here), and `reembed_mode(true)` bypasses the contract check by its
    // own mechanism, never by flipping the override.
    let config = backends.config.clone();
    let store: std::sync::Arc<dyn crate::store::GraphStore> = Arc::from(backends.store);
    let embedder: std::sync::Arc<dyn Embedder> = Arc::from(backends.embedder);
    let live_contract: EmbeddingContract = backends.embedding.clone();

    let mem = Memory::builder()
        .session(&args.session)
        .agent(&args.agent)
        .config(config)
        .store(store.clone())
        .embedder(embedder.clone())
        .embedding_contract(live_contract.clone())
        .reembed_mode(true)
        .build()
        .await
        .map_err(map_writer_err)?;

    // Snapshot concept ids + contents under the graph READ lock, sorted by id
    // for deterministic embed order, then DROP the lock: the embed calls below
    // are real model inference and must never run under any lock.
    let concepts: Vec<(NodeId, String)> = {
        let g = mem.graph().read();
        let mut snapshot: Vec<(NodeId, String)> =
            g.concepts().map(|c| (c.id, c.content.clone())).collect();
        snapshot.sort_by_key(|(id, _)| id.0);
        snapshot
    };

    // Every path below — success or any mid-run abort — funnels into
    // close_writer at the end of this function (K2-R1-6): a failed migration
    // must release the writer lease immediately, not hold it to TTL.
    let out = if concepts.is_empty() {
        Ok(format!(
            "session '{}' has no concepts; nothing to re-embed",
            args.session
        ))
    } else {
        rewrite_all(
            &mem,
            &args.session,
            &args.agent,
            &embedder,
            &live_contract,
            &concepts,
        )
        .await
    };
    close_writer(mem, out).await
}

/// The all-or-nothing rewrite body: embed EVERY concept BEFORE mutating the
/// graph. Any embed failure aborts here — no mutation appended, no partial
/// rewrite, no mixed-space state — and the caller's `close_writer` still
/// releases the lease on this error path (K2-R1-6).
async fn rewrite_all(
    mem: &Memory,
    session: &str,
    agent: &str,
    embedder: &Arc<dyn Embedder>,
    live_contract: &EmbeddingContract,
    concepts: &[(NodeId, String)],
) -> Result<String, CliError> {
    let mut updates: Vec<(NodeId, Vec<f32>)> = Vec::with_capacity(concepts.len());
    for (id, content) in concepts {
        let vector = embedder.embed(content).await.map_err(|e| {
            CliError::Runtime(format!(
                "re-embed aborted BEFORE mutating the graph: embedding concept {id} \
                 ({content:?}) failed: {e}"
            ))
        })?;
        updates.push((*id, vector));
    }

    // Under the graph WRITE lock: capture before-coverage, append the whole
    // ordered batch, capture after-coverage. Deliberately NO manual drain:
    // close_writer's Memory::close final flush drains this exact log —
    // UpsertNodes then SetEmbedding — and flushes it transactionally with
    // the lease token before releasing the lease. The write lock makes the
    // append sequence atomic against the flush task's drain_log, so no
    // partial batch can ever be flushed between the two coverages.
    let total = concepts.len();
    debug_assert_eq!(mem.session().as_str(), session);
    let (before, after) = {
        let mut g = mem.graph().write();
        let before = g.concepts().filter(|c| c.embedding.is_some()).count();
        g.reembed_all(updates, live_contract.clone())
            .map_err(CliError::from)?;
        let after = g.concepts().filter(|c| c.embedding.is_some()).count();
        (before, after)
    };

    Ok(format!(
        "re-embedded session '{session}' as agent '{agent}': {total} concept(s) rewritten into \
         the live space\n\
         coverage: embedded {before}/{total} -> {after}/{total}\n\
         contract now: kind={} model={} dim={}\n\
         the vector rewrite and contract swap flush as ONE transaction with this \
         writer's lease release",
        live_contract.kind,
        live_contract.model.as_deref().unwrap_or("<unset>"),
        live_contract.dim,
    ))
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::store::{
        lease::{LeaseHolder, LeaseOutcome},
        GraphStore,
    };
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, GraphSnapshot, Interaction, Mutation,
        MutationBatch, NodeId, SessionId, StoreError,
    };
    use crate::{embed::FixtureEmbedder, MemoryStore};
    use crate::{
        embed::{EmbedderConfig, EmbedderKind},
        store::{StoreConfig, StoreKind},
    };
    use std::time::Duration;

    const SESSION: &str = "k2-reembed";
    const AGENT: &str = "operator";

    /// `Arc<MemoryStore>` as a `GraphStore` so the seeded store is shared by
    /// identity between the seed phase and the verb under test (the same
    /// two-processes-one-file shape the sibling CLI tests model).
    struct ArcStore(Arc<MemoryStore>);

    #[async_trait::async_trait]
    impl GraphStore for ArcStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.0.init_schema().await
        }
        fn capabilities(&self) -> crate::store::Capabilities {
            self.0.capabilities()
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.0.vector_dimensions()
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.0.flush(batch, token).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.0.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<crate::types::Scored<NodeId>>, StoreError> {
            self.0.keyword_candidates(session, tokens, limit).await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            self.0.blast_radius(session, node, min_edge_age, now).await
        }
        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.0.interaction_span(session, node, min_age, now).await
        }
        async fn acquire_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.0.acquire_lease(session, holder, ttl).await
        }
        async fn refresh_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.0.refresh_lease(session, holder, ttl).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
        ) -> Result<(), StoreError> {
            self.0.release_lease(session, holder).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<crate::types::Scored<NodeId>>, StoreError> {
            self.0.vector_candidates(session, embedding, limit).await
        }
        async fn record_canonization(
            &self,
            event: &crate::types::CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.0.record_canonization(event, token).await
        }
    }

    fn backends_on(store: Arc<MemoryStore>, kind: &str, model: &str) -> ResolvedBackends {
        ResolvedBackends {
            store: Box::new(ArcStore(store)),
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
                vector_dim: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
                ..Default::default()
            },
            embedding: EmbeddingContract {
                kind: kind.into(),
                model: Some(model.into()),
                dim: 1024,
            },
            allow_embedding_mismatch: false,
            config: crate::Config::default(),
        }
    }

    /// Plant the pre-K2 damage shape directly in the durable store: one
    /// interaction, two concepts under a LEGACY contract — one WITH a legacy
    /// vector, one with a NULL embedding (the row an interrupted or degraded
    /// writer leaves behind).
    async fn seed_damaged_session(store: &Arc<MemoryStore>) {
        let sid = SessionId::from(SESSION);
        let seeder = LeaseHolder::for_this_process(&AgentId::from("seeder"));
        let LeaseOutcome::Acquired(info) = store
            .acquire_lease(&sid, &seeder, Duration::from_secs(30))
            .await
            .unwrap()
        else {
            panic!("test setup must acquire the seed lease");
        };
        let ts = chrono::Utc::now();
        let i1 = NodeId::new();
        let concept = |id: NodeId, content: &str, emb: Option<Vec<f32>>| Mutation::UpsertNode {
            node: crate::types::Node::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: content.into(),
                canonical_key: content.to_lowercase(),
                concept_type: ConceptType::Entity,
                origin_interaction: i1,
                origin_agent: AgentId::from("seeder"),
                created_at: ts,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: emb,
                human_confirmed: 0,
                chunk_group_id: None,
            }),
        };
        let c1 = NodeId::new();
        let c2 = NodeId::new();
        let derives_edge = |target: NodeId| Mutation::UpsertEdge {
            edge: crate::types::Edge {
                event_time: None,
                id: NodeId::new(),
                session_id: sid.clone(),
                source: i1,
                target,
                edge_type: crate::types::EdgeType::Derives,
                weight: 1.0,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
            },
        };
        // Contract FIRST: the store treats a contract stamped over already
        // stored vectors as a legacy upgrade and strips them (untrusted
        // provenance) — which would manufacture damage instead of modeling it.
        let set_contract = Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(EmbeddingContract {
                kind: "legacy".into(),
                model: Some("v1".into()),
                dim: 1024,
            }),
        };
        let interaction = Mutation::UpsertNode {
            node: crate::types::Node::Interaction(Interaction {
                event_time: None,
                id: i1,
                session_id: sid.clone(),
                agent_id: AgentId::from("seeder"),
                prompt_text: Some("seed".into()),
                previous_id: None,
                created_at: ts,
            }),
        };
        let batch = MutationBatch {
            mutations: vec![
                interaction,
                set_contract,
                concept(c1, "user schema", Some(vec![0.25_f32; 1024])),
                derives_edge(c1),
                concept(c2, "auth middleware", None),
                derives_edge(c2),
            ],
        };
        store.flush(&batch, Some(info.token)).await.unwrap();
        store.release_lease(&sid, &seeder).await.unwrap();
    }

    /// Durable reader-side assertions: stamped contract kind plus per-concept
    /// vector widths (None == NULL row).
    async fn durable_state(store: &Arc<MemoryStore>) -> (Option<String>, Vec<Option<usize>>) {
        let loaded = crate::cli::load_reader_graph(store.as_ref(), SESSION)
            .await
            .unwrap();
        let g = loaded.graph.read();
        let mut widths = g
            .concepts()
            .map(|c| c.embedding.as_ref().map(|v| v.len()))
            .collect::<Vec<_>>();
        widths.sort();
        (g.embedding().map(|c| c.kind.clone()), widths)
    }

    #[tokio::test]
    async fn re_embed_fills_null_rows_and_stamps_the_new_contract() {
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        // Pre-state sanity: the durable session is genuinely damaged.
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("legacy"));
        assert_eq!(widths, vec![None, Some(1024)]);

        let out = run(
            backends_on(store.clone(), "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
            },
        )
        .await
        .expect("re-embed");

        // The NULL row is filled and the live contract is stamped.
        assert!(out.contains("coverage: embedded 1/2 -> 2/2"), "{out}");
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("fixture"), "contract must migrate");
        assert_eq!(widths, vec![Some(1024), Some(1024)]);
    }

    #[tokio::test]
    async fn re_embed_refuses_the_allow_embedding_mismatch_override() {
        let err = run(
            backends_on(Arc::new(MemoryStore::new()), "fixture", "m"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: true,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert!(
            err.to_string().contains("re-embed IS the embedding"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn re_embed_on_an_empty_session_is_an_honest_no_op() {
        let store = Arc::new(MemoryStore::new());
        let out = run(
            backends_on(store, "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
            },
        )
        .await
        .expect("an empty session is Ok, not an error");
        assert!(out.contains("no concepts; nothing to re-embed"), "{out}");
    }

    #[tokio::test]
    async fn re_embed_reports_a_held_lease_as_a_conflict() {
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;

        // Another writer owns the session: the honest, named refusal.
        let other = LeaseHolder::for_this_process(&AgentId::from("another-writer"));
        match store
            .acquire_lease(&SessionId::from(SESSION), &other, Duration::from_secs(30))
            .await
            .unwrap()
        {
            LeaseOutcome::Acquired(_) => {}
            other => panic!("test setup must hold the lease, got {other:?}"),
        }

        let err = run(
            backends_on(store, "fixture", "fixture-model"),
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already held"), "{err}");
    }

    /// An embedder whose every embed fails — drives the mid-run abort path.
    struct FailingEmbedder;

    #[async_trait::async_trait]
    impl Embedder for FailingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            Err(crate::embed::EmbedError::Backend("failing embedder".into()))
        }
    }

    #[tokio::test]
    async fn re_embed_embed_failure_still_releases_the_lease() {
        // K2-R1-6: the old code returned from run() on an embed failure
        // WITHOUT close_writer, so the session stayed unwritable for the rest
        // of the TTL. The abort message is honest AND the lease goes back.
        let store = Arc::new(MemoryStore::new());
        seed_damaged_session(&store).await;
        let mut backends = backends_on(store.clone(), "fixture", "m");
        backends.embedder = Box::new(FailingEmbedder);

        let err = run(
            backends,
            Args {
                session: SESSION.into(),
                agent: AGENT.into(),
                allow_embedding_mismatch: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("re-embed aborted BEFORE mutating the graph"),
            "{err}"
        );

        // Durable state untouched: old contract and old vectors survive.
        let (kind, widths) = durable_state(&store).await;
        assert_eq!(kind.as_deref(), Some("legacy"));
        assert_eq!(widths, vec![None, Some(1024)]);

        // The point of the fix: a DIFFERENT writer can take the lease right
        // now, not after the TTL.
        let verifier = LeaseHolder::for_this_process(&AgentId::from("lease-verifier"));
        match store
            .acquire_lease(
                &SessionId::from(SESSION),
                &verifier,
                Duration::from_secs(30),
            )
            .await
            .unwrap()
        {
            LeaseOutcome::Acquired(_) => {}
            other => panic!("lease must be released after a failed migration, got {other:?}"),
        }
    }
}
