//! CLI surface (spec §6.2) — read verbs are lease-free reader processes;
//! write verbs open exactly one [`Memory`] (which acquires the T8.6 writer
//! lease), perform the op, and [`Memory::close`].
//!
//! Validators and inspect resolution live here so MCP can share them without
//! a `cli` → `mcp` cycle.

pub mod caps;
pub mod demo;
pub mod derive;
pub mod inspect;
pub mod provision;
pub mod recall;
pub mod record_action;
pub mod reserve;
pub mod saints;
pub mod serve_web;
pub mod stats;

pub use caps::{check_size, CliError, ConceptKind};

use std::sync::Arc;

use parking_lot::RwLock;

use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::store::GraphStore;
use crate::types::{LamboError, SessionId};

/// Session materialized for a lease-free reader command.
pub(crate) struct LoadedReader {
    pub graph: Arc<RwLock<Graph>>,
    pub index: Arc<RwLock<InvertedIndex>>,
    pub session: SessionId,
}

/// Load a session as a reader process (spec §2.2). Never touches the lease.
///
/// Uses the async core, not the sync `load_session` wrapper (that parks a
/// worker thread). A missing session is a first use — empty graph + index.
pub(crate) async fn load_reader_graph(
    store: &dyn GraphStore,
    session: &str,
) -> Result<LoadedReader, CliError> {
    let session = SessionId::new(session);
    let loaded = crate::store::load::load_session_async(store, &session)
        .await
        .map_err(|e| CliError::Runtime(e.to_string()))?;
    Ok(LoadedReader {
        graph: Arc::new(RwLock::new(loaded.graph)),
        index: Arc::new(RwLock::new(loaded.index)),
        session,
    })
}

/// Open exactly one writer via [`Memory::build`] (acquires the T8.6 lease).
///
/// A [`LamboError::Conflict`] is returned as-is — it already names the holder,
/// age, and `OPERATOR_OVERRIDE`.
pub(crate) async fn open_writer(
    backends: ResolvedBackends,
    session: &str,
    agent: &str,
) -> Result<Memory, CliError> {
    Memory::builder()
        .session(session)
        .agent(agent)
        .backends(backends)
        .build()
        .await
        .map_err(map_writer_err)
}

/// [`Memory::close`] after a write, so the lease is released and the tail is
/// durable. A failed close is always surfaced (retryable per T8.1) even when
/// the op itself succeeded.
pub(crate) async fn close_writer(
    mem: Memory,
    out: Result<String, CliError>,
) -> Result<String, CliError> {
    let close = mem.close().await;
    match (out, close) {
        (Ok(s), Ok(())) => Ok(s),
        (Err(e), Ok(())) => Err(e),
        (Ok(_), Err(e)) => Err(CliError::Runtime(format!("close: {e}"))),
        (Err(e), Err(close_err)) => Err(CliError::Runtime(format!(
            "{e}; close also failed: {close_err}"
        ))),
    }
}

fn map_writer_err(err: LamboError) -> CliError {
    // Conflict already names the holder / age / override. Keep the Display
    // text so subprocess tests can match `single-writer` and the agent token.
    CliError::Runtime(err.to_string())
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::cli::caps::{check_size, MAX_CONTENT_BYTES};
    use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder};
    use crate::mcp::server::{
        DeriveParams, RecallParams, RecordActionParams, WireConcept, WireConceptType, WireParentOf,
    };
    use crate::mcp::LamboServer;
    use crate::store::lease::{LeaseHolder, LeaseOutcome};
    use crate::store::{Capabilities, StoreConfig, StoreKind};
    use crate::types::{
        CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch,
        NodeId, Scored, StoreError,
    };
    use crate::MemoryStore;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use std::time::Duration;

    /// `Arc<MemoryStore>` as a `GraphStore` so sequential CLI commands can
    /// share one in-RAM store the way two process invocations share a file.
    /// The second field overrides advertised capabilities (T83-5: claim
    /// `VECTOR_SEARCH` while the inner store has none).
    #[derive(Clone)]
    struct SharedMemory(Arc<MemoryStore>, Capabilities);

    #[async_trait]
    impl GraphStore for SharedMemory {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.0.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.1
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.0.vector_dimensions()
        }
        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.0.flush(batch).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.0.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.vector_candidates(session, embedding, limit).await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.0.blast_radius(session, node, min_edge_age, now).await
        }
        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<InteractionSpan, StoreError> {
            self.0.interaction_span(session, node, min_age, now).await
        }
        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.0.record_canonization(event).await
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
    }

    fn backends_on(store: Arc<MemoryStore>) -> ResolvedBackends {
        ResolvedBackends {
            store: Box::new(SharedMemory(store, Capabilities::empty())),
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
        }
    }

    fn hit_contents(context: &str) -> Vec<String> {
        // T5.3 hit lines start with the concept text before ` [`.
        let mut out: Vec<String> = context
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('⚑') {
                    return None;
                }
                line.split(" [").next().map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[tokio::test]
    async fn cli_mcp_differential_derive_record_recall() {
        // Isolated sessions, same ops via CLI command functions vs LamboServer
        // tools. Compare recall *content* (concept texts), not UUIDs/timestamps.
        let cli_store = Arc::new(MemoryStore::new());
        let mcp_store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mcp_mem = Memory::builder()
            .session("cli-mcp-diff-mcp")
            .agent("agent-a")
            .store(mcp_store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn crate::embed::Embedder>)
            .embedding_contract(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .flush_interval(Duration::from_secs(3_600))
            .build()
            .await
            .expect("mcp memory");
        let server = LamboServer::new(Arc::new(mcp_mem));

        let derived = crate::cli::derive::run(
            backends_on(cli_store.clone()),
            crate::cli::derive::Args {
                session: "cli-mcp-diff-cli".into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec!["auth middleware:user schema".into()],
                concept: vec!["auth middleware:entity".into()],
            },
        )
        .await
        .expect("cli derive");
        assert!(derived.contains("created"), "{derived}");

        let mcp_derived = server
            .derive_impl(DeriveParams {
                agent_id: "agent-a".into(),
                concepts: vec![
                    WireConcept {
                        content: "user schema".into(),
                        concept_type: WireConceptType::Entity,
                    },
                    WireConcept {
                        content: "auth middleware".into(),
                        concept_type: WireConceptType::Entity,
                    },
                ],
                parent_of: Some(vec![WireParentOf {
                    parent: "user schema".into(),
                    child: "auth middleware".into(),
                }]),
            })
            .await;
        assert_eq!(mcp_derived.is_error, Some(false), "{mcp_derived:?}");

        let recorded = crate::cli::record_action::run(
            backends_on(cli_store.clone()),
            crate::cli::record_action::Args {
                session: "cli-mcp-diff-cli".into(),
                agent: "agent-a".into(),
                action: "create user".into(),
                produces: vec!["user schema".into()],
                modifies: vec![],
                depends_on: vec!["auth middleware".into()],
            },
        )
        .await
        .expect("cli record-action");
        assert!(recorded.contains("create user"), "{recorded}");

        let mcp_action = server
            .record_action_impl(RecordActionParams {
                agent_id: "agent-a".into(),
                action: "create user".into(),
                produces: Some(vec!["user schema".into()]),
                modifies: None,
                depends_on: Some(vec!["auth middleware".into()]),
            })
            .await;
        assert_eq!(mcp_action.is_error, Some(false), "{mcp_action:?}");

        let cli_ctx = crate::cli::recall::run(
            &backends_on(cli_store.clone()),
            "cli-mcp-diff-cli",
            "update user schema",
            Some(5),
            Some(500),
            Some(2),
        )
        .await
        .expect("cli recall");
        let mcp_recall = server
            .recall_impl(RecallParams {
                agent_id: "agent-a".into(),
                query: "update user schema".into(),
                top_k: Some(5),
                max_tokens: Some(500),
                traversal_depth: Some(2),
            })
            .await;
        assert_eq!(mcp_recall.is_error, Some(false), "{mcp_recall:?}");
        let mcp_ctx = match &mcp_recall.content[0] {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text, got {other:?}"),
        };

        let cli_hits = hit_contents(&cli_ctx);
        let mcp_hits = hit_contents(&mcp_ctx);
        assert_eq!(
            cli_hits, mcp_hits,
            "CLI and MCP recall must name the same concepts\nCLI:\n{cli_ctx}\nMCP:\n{mcp_ctx}"
        );
        for needle in ["user schema", "auth middleware", "create user"] {
            assert!(
                cli_ctx.contains(needle) && mcp_ctx.contains(needle),
                "both surfaces must mention {needle}\nCLI:\n{cli_ctx}\nMCP:\n{mcp_ctx}"
            );
        }

        server.memory().close().await.expect("mcp close");
    }

    #[tokio::test]
    async fn cli_refuses_oversized_and_control_char_like_mcp() {
        let store = Arc::new(MemoryStore::new());
        let big = "A".repeat(MAX_CONTENT_BYTES + 1);
        let err = crate::cli::derive::run(
            backends_on(store.clone()),
            crate::cli::derive::Args {
                session: "cli-caps".into(),
                agent: "agent-a".into(),
                content: big,
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec![],
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)), "{err}");
        assert!(err.to_string().contains("exceeds"), "{err}");

        let err = crate::cli::derive::run(
            backends_on(store),
            crate::cli::derive::Args {
                session: "cli-caps".into(),
                agent: "agent-a".into(),
                content: "ok\u{0001}no".into(),
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec![],
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("U+0001"), "{err}");
        assert!(!err.to_string().contains('\u{0001}'), "{err}");
    }

    #[test]
    fn shared_validators_are_the_caps_module() {
        // Once extracted, MCP and CLI cannot drift: there is one check_size.
        let err = check_size("query", "ok\u{0001}no").unwrap_err();
        assert!(err.contains("U+0001"), "{err}");
        let big = "A".repeat(MAX_CONTENT_BYTES + 1);
        assert!(check_size("content", &big).is_err());
        assert_eq!(MAX_CONTENT_BYTES, crate::cli::caps::MAX_CONTENT_BYTES);
    }

    #[tokio::test]
    async fn parent_of_writes_hierarchical_edge_parent_to_child() {
        let store = Arc::new(MemoryStore::new());
        crate::cli::derive::run(
            backends_on(store.clone()),
            crate::cli::derive::Args {
                session: "t83-parent-of".into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec!["auth middleware:user schema".into()],
                concept: vec!["auth middleware:entity".into()],
            },
        )
        .await
        .expect("derive");

        let loaded =
            load_reader_graph(&SharedMemory(store, Capabilities::empty()), "t83-parent-of")
                .await
                .expect("load");
        let g = loaded.graph.read();
        let parent = g
            .concepts()
            .find(|c| c.content == "user schema")
            .map(|c| c.id)
            .expect("parent");
        let child = g
            .concepts()
            .find(|c| c.content == "auth middleware")
            .map(|c| c.id)
            .expect("child");
        assert!(
            g.edge_between(parent, child, crate::types::EdgeType::Hierarchical)
                .is_some(),
            "Hierarchical edge must run parent → child (right of colon → left of --parent-of)"
        );
        assert!(
            g.edge_between(child, parent, crate::types::EdgeType::Hierarchical)
                .is_none(),
            "an inverted parent_of map must fail this test"
        );
    }

    #[tokio::test]
    async fn reader_recall_does_not_spawn_gc_or_mutate_epoch() {
        let src = include_str!("recall.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            !prod.contains(".spawn()") && !prod.contains("Daemon::spawn"),
            "readers never spawn GC (spawn = writer); adding daemon.spawn() in recall.rs must fail this test"
        );

        let store = Arc::new(MemoryStore::new());
        crate::cli::derive::run(
            backends_on(store.clone()),
            crate::cli::derive::Args {
                session: "t83-no-gc".into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec![],
            },
        )
        .await
        .expect("derive");

        let before = load_reader_graph(
            &SharedMemory(store.clone(), Capabilities::empty()),
            "t83-no-gc",
        )
        .await
        .expect("load before");
        let epoch_before = before.graph.read().epoch();
        let statuses_before: Vec<_> = {
            let g = before.graph.read();
            let mut v: Vec<_> = g
                .concepts()
                .map(|c| (c.id, c.canonization_status))
                .collect();
            v.sort_by_key(|(id, _)| id.0);
            v
        };

        crate::cli::recall::run(
            &backends_on(store.clone()),
            "t83-no-gc",
            "user schema",
            Some(5),
            Some(500),
            Some(2),
        )
        .await
        .expect("recall");

        let after = load_reader_graph(&SharedMemory(store, Capabilities::empty()), "t83-no-gc")
            .await
            .expect("load after");
        let epoch_after = after.graph.read().epoch();
        let statuses_after: Vec<_> = {
            let g = after.graph.read();
            let mut v: Vec<_> = g
                .concepts()
                .map(|c| (c.id, c.canonization_status))
                .collect();
            v.sort_by_key(|(id, _)| id.0);
            v
        };
        assert_eq!(
            epoch_after, epoch_before,
            "reader recall must not mutate graph epoch"
        );
        assert_eq!(
            statuses_after, statuses_before,
            "reader recall must not mutate canonization statuses"
        );
    }

    struct FailingEmbedder;

    #[async_trait]
    impl crate::embed::Embedder for FailingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            Err(crate::embed::EmbedError::Unavailable("down".into()))
        }
    }

    #[tokio::test]
    async fn recall_prints_skipped_vector_leg_when_embed_fails() {
        let store = Arc::new(MemoryStore::new());
        crate::cli::derive::run(
            backends_on(store.clone()),
            crate::cli::derive::Args {
                session: "t83-vector-skip".into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec![],
            },
        )
        .await
        .expect("derive");

        let backends = ResolvedBackends {
            store: Box::new(SharedMemory(store, Capabilities::VECTOR_SEARCH)),
            embedder: Box::new(FailingEmbedder),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
        };
        let out = crate::cli::recall::run(
            &backends,
            "t83-vector-skip",
            "user schema",
            Some(5),
            Some(500),
            Some(2),
        )
        .await
        .expect("recall");
        assert!(
            out.contains("vector leg skipped"),
            "operator must see the skipped vector leg: {out}"
        );
        assert!(
            out.contains("query embedding failed"),
            "operator must see the embed failure: {out}"
        );
    }
}

#[cfg(all(test, feature = "store-sqlite", feature = "embed-fixture"))]
mod sqlite_tests {
    use super::*;
    use crate::cli::caps::ConceptKind;
    use crate::resolve::resolve_from_config_path;
    use crate::store::StoreKind;

    const ENV_KEYS: &[&str] = &[
        "LAMBO_STORE",
        "LAMBO_EMBEDDER",
        "LAMBO_CONFIG",
        "LAMBO_COCKROACH_DSN",
        "DATABASE_URL",
        "LAMBO_SQLITE_PATH",
        "LAMBO_EMBED_DIM",
        "LAMBO_LLAMA_EMBED_URL",
        "LAMBO_LLAMA_MODEL",
    ];

    fn scratch() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "lambo-cli-sqlite-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.sqlite");
        let cfg = dir.join("lambo.toml");
        std::fs::write(
            &cfg,
            format!(
                "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
                db.display()
            ),
        )
        .unwrap();
        (dir, cfg)
    }

    fn resolve_clean(cfg: &std::path::Path) -> ResolvedBackends {
        let _g = crate::test_util::env_lock();
        for k in ENV_KEYS {
            std::env::remove_var(k);
        }
        resolve_from_config_path(Some(cfg)).expect("resolve sqlite")
    }

    #[tokio::test]
    async fn provision_then_every_subcommand_against_sqlite() {
        let (dir, cfg) = scratch();
        let session = "t83-sqlite";

        let store = {
            let _g = crate::test_util::env_lock();
            for k in ENV_KEYS {
                std::env::remove_var(k);
            }
            let file = crate::LamboFile::load_resolved(Some(&cfg)).expect("file");
            assert_eq!(file.store.kind, StoreKind::Sqlite);
            crate::resolve_store_only(Some(&cfg)).expect("store")
        };
        let out = crate::cli::provision::run(store, StoreKind::Sqlite)
            .await
            .expect("provision");
        assert!(out.contains("sqlite"), "{out}");

        let derived = crate::cli::derive::run(
            resolve_clean(&cfg),
            crate::cli::derive::Args {
                session: session.into(),
                agent: "agent-a".into(),
                content: "user schema".into(),
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec!["auth middleware:entity".into()],
            },
        )
        .await
        .expect("derive");
        assert!(derived.contains("created"), "{derived}");

        let recorded = crate::cli::record_action::run(
            resolve_clean(&cfg),
            crate::cli::record_action::Args {
                session: session.into(),
                agent: "agent-a".into(),
                action: "create user".into(),
                produces: vec!["user schema".into()],
                modifies: vec![],
                depends_on: vec!["auth middleware".into()],
            },
        )
        .await
        .expect("record-action");
        assert!(recorded.contains("create user"), "{recorded}");

        let ctx = crate::cli::recall::run(
            &resolve_clean(&cfg),
            session,
            "update user schema",
            Some(5),
            None,
            None,
        )
        .await
        .expect("recall");
        assert!(ctx.contains("user schema"), "{ctx}");

        let saints = crate::cli::saints::run(resolve_clean(&cfg).store.as_ref(), session)
            .await
            .expect("saints");
        assert!(saints.contains(session), "{saints}");

        let view = crate::cli::inspect::run(
            resolve_clean(&cfg).store.as_ref(),
            session,
            "user schema",
            2,
        )
        .await
        .expect("inspect");
        assert!(view.contains("user schema"), "{view}");

        let stats = crate::cli::stats::run(resolve_clean(&cfg).store.as_ref(), session)
            .await
            .expect("stats");
        assert!(stats.contains("nodes="), "{stats}");
        assert!(
            stats.contains("n/a") && stats.contains("writer-only"),
            "{stats}"
        );

        let backends = resolve_clean(&cfg);
        let loaded = load_reader_graph(backends.store.as_ref(), session)
            .await
            .expect("load");
        let node = {
            let g = loaded.graph.read();
            let id = g
                .concepts()
                .find(|c| c.content == "user schema")
                .map(|c| c.id)
                .expect("user schema concept");
            id
        };
        let reserved = crate::cli::reserve::reserve(
            resolve_clean(&cfg),
            crate::cli::reserve::ReserveArgs {
                session: session.into(),
                agent: "agent-a".into(),
                node: node.0.to_string(),
                ttl_seconds: Some(30),
            },
        )
        .await
        .expect("reserve");
        assert!(reserved.contains("advisory"), "{reserved}");
        assert!(
            reserved.contains("this process exits"),
            "reserve must say the lock ends when the process exits: {reserved}"
        );
        assert!(
            !reserved.contains("lost on restart"),
            "must not blame restart for a lock that is already gone: {reserved}"
        );

        // Reservations are RAM-local (S5) and die with close(), so a second
        // CLI invocation cannot release what the first reserved. The command
        // still has to exist (MCP parity) and must fail closed, not panic.
        let released = crate::cli::reserve::release(
            resolve_clean(&cfg),
            crate::cli::reserve::ReleaseArgs {
                session: session.into(),
                agent: "agent-a".into(),
                node: node.0.to_string(),
            },
        )
        .await;
        assert!(
            released.is_err(),
            "release after a closed reserve must not invent a lock: {released:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
