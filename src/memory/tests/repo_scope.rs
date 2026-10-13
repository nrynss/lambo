//! Generic multi-repository identity tests, with no rig data.
use super::*;
use crate::graph::resource::scoped_resource_key;
use crate::test_util::VectorSearchable;
use crate::types::{ConceptCitations, EdgeType, ReferenceRefusalKind};
use crate::writeq::RECEIPT_WAIT_MAX;

async fn memory(session: &str, strategy: MatchStrategy) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        // Ranked in the holder's graph, so a merge sees unflushed vectors.
        .store(
            Arc::new(super::writes::VectorSearchStore::graph_ranked(Arc::new(
                MemoryStore::new(),
            ))) as Arc<dyn GraphStore>,
        )
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(strategy)
        .flush_interval(Duration::from_secs(3600))
        .build()
        .await
        .unwrap()
}

fn path_id(mem: &Memory, repo: &str, path: &str) -> NodeId {
    mem.graph
        .read()
        .concept_by_key(&scoped_resource_key(path, Some(repo)).unwrap())
        .unwrap()
}

#[tokio::test]
async fn repo_scope_isolates_paths_and_keeps_legacy_and_nonresources() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-identity-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        mem.derive_as(
            &agent,
            &[("src/main.rs", ConceptType::Resource)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        for repo in ["repo-a", "repo-b"] {
            let context = ParentOf::none().with_repo_scope(Some(repo));
            let out = mem
                .derive_as(
                    &agent,
                    &[
                        ("src/main.rs", ConceptType::Resource),
                        ("src/Main.rs", ConceptType::Resource),
                        ("docs/reference", ConceptType::Resource),
                        ("src/a_b.rs", ConceptType::Resource),
                        ("src/b_a.rs", ConceptType::Resource),
                    ],
                    &context,
                )
                .await
                .unwrap();
            assert_eq!(out.created.len(), 5);
            assert!(out.semantic_merged.is_empty());
            let repeat = mem
                .derive_as(&agent, &[("src/main.rs", ConceptType::Resource)], &context)
                .await
                .unwrap();
            assert_eq!(repeat.matched, [path_id(&mem, repo, "src/main.rs")]);
            assert!(repeat.created.is_empty());
        }
        assert_ne!(
            path_id(&mem, "repo-a", "src/main.rs"),
            path_id(&mem, "repo-b", "src/main.rs")
        );
        assert_ne!(
            path_id(&mem, "repo-a", "src/main.rs"),
            path_id(&mem, "repo-a", "src/Main.rs")
        );
        assert_eq!(mem.graph.read().concepts().count(), 11);
        // Explicit non-Resources retain the legacy canonical key.
        let out = mem
            .derive_as(
                &agent,
                &[("config.toml", ConceptType::Logic)],
                &ParentOf::none().with_repo_scope(Some("repo-a")),
            )
            .await
            .unwrap();
        let logic = out.created[0];
        assert_eq!(
            mem.graph
                .read()
                .concept_by_key(&crate::graph::canonical::canonical_key(
                    "config.toml",
                    |_| None
                )),
            Some(logic)
        );
        assert!(scoped_resource_key("a decision", Some("repo-a")).is_none());
        mem.close().await.unwrap();
    }
}

#[tokio::test]
async fn queued_actions_and_derives_share_scoped_file_dependencies() {
    let mem = memory("repo-queued-actions", MatchStrategy::Hybrid).await;
    let agent = AgentId::new("agent-a");
    for repo in ["repo-a", "repo-b"] {
        let action = Action {
            repo_scope: Some(repo),
            event_time: None,
            action: "built the binary",
            produces: &["src/main.rs"],
            modifies: &[],
            depends_on: &[],
        };
        let submitted = mem.record_action_async_as(&agent, &action).await.unwrap();
        let answer = mem
            .pipeline()
            .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
            .await;
        assert!(answer.is_settled(), "{}", answer.describe());
        assert_ne!(answer.tag(), "failed", "{}", answer.describe());
    }
    let a = path_id(&mem, "repo-a", "src/main.rs");
    let b = path_id(&mem, "repo-b", "src/main.rs");
    assert_ne!(a, b);
    let action = Action {
        repo_scope: Some("repo-b"),
        event_time: None,
        action: "checked the artifact",
        produces: &[],
        modifies: &[],
        depends_on: &["src/main.rs"],
    };
    let out = mem
        .record_action_embedded_as(&agent, &action)
        .await
        .unwrap();
    assert!(mem
        .graph
        .read()
        .edge_between(out.action_node, b, EdgeType::Dependency)
        .is_some());
    assert!(mem
        .graph
        .read()
        .edge_between(out.action_node, a, EdgeType::Dependency)
        .is_none());
    let citations = [ConceptCitations {
        against: Vec::new(),
        content: "artifact has been checked".into(),
        depends_on: vec!["src/main.rs".into()],
    }];
    let submitted = mem
        .derive_cited_async_as(
            &agent,
            &[("artifact has been checked", ConceptType::Observation)],
            &ParentOf::none().with_repo_scope(Some("repo-a")),
            &citations,
            None,
        )
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert_ne!(answer.tag(), "failed", "{}", answer.describe());
    let source = mem
        .graph
        .read()
        .concepts()
        .find(|c| c.content == "artifact has been checked")
        .unwrap()
        .id;
    assert!(mem
        .graph
        .read()
        .edge_between(source, a, EdgeType::Dependency)
        .is_some());
    mem.close().await.unwrap();
}

#[tokio::test]
async fn scoped_citations_refuse_legacy_fallback_but_allow_explicit_ids() {
    let mem = memory("repo-citation-fallback", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let old = mem
        .derive_as(
            &agent,
            &[("README.md", ConceptType::Resource)],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    let context = ParentOf::none().with_repo_scope(Some("repo-a"));
    let concepts = [("documented the build", ConceptType::Logic)];
    let citations = [ConceptCitations {
        against: Vec::new(),
        content: concepts[0].0.into(),
        depends_on: vec!["README.md".into()],
    }];
    let before = mem.graph.read().snapshot();
    let err = mem
        .derive_cited_as(&agent, &concepts, &context, &citations)
        .await
        .unwrap_err();
    assert!(matches!(err, LamboError::Reference(r) if r.kind == ReferenceRefusalKind::Missing));
    assert_eq!(mem.graph.read().snapshot(), before);
    let explicit = [ConceptCitations {
        against: Vec::new(),
        content: concepts[0].0.into(),
        depends_on: vec![old.to_string()],
    }];
    let out = mem
        .derive_cited_as(&agent, &concepts, &context, &explicit)
        .await
        .unwrap();
    assert_eq!(out.cited, 1);
    mem.close().await.unwrap();
}

#[tokio::test]
async fn scoped_hierarchy_uses_exact_paths_and_refuses_invalid_scope_before_write() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-hierarchy-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        let pairs = [("docs/reference", "docs/reference/api.md")];
        for repo in ["repo-a", "repo-b"] {
            let context = ParentOf::from_pairs(&pairs).with_repo_scope(Some(repo));
            mem.derive_as(
                &agent,
                &[("hierarchy complete", ConceptType::Logic)],
                &context,
            )
            .await
            .unwrap();
            let parent = path_id(&mem, repo, pairs[0].0);
            let child = path_id(&mem, repo, pairs[0].1);
            assert!(mem
                .graph
                .read()
                .edge_between(parent, child, EdgeType::Hierarchical)
                .is_some());
        }
        let before = mem.graph.read().snapshot();
        assert!(mem
            .derive_async_as(
                &agent,
                &[("a.rs", ConceptType::Resource)],
                &ParentOf::none().with_repo_scope(Some("  ")),
                None
            )
            .await
            .is_err());
        assert_eq!(mem.graph.read().snapshot(), before);
        mem.close().await.unwrap();
    }
}

fn hybrid_on(
    inner: &Arc<MemoryStore>,
    session: &str,
    embedder: Arc<dyn Embedder>,
) -> MemoryBuilder {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(VectorSearchable(inner.clone())) as Arc<dyn GraphStore>)
        .embedder(embedder)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
}

async fn settle_all(
    mem: &Memory,
    agent: &AgentId,
    receipts: &[crate::writeq::ReceiptId],
) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let tags: Vec<String> = receipts
            .iter()
            .map(|r| mem.pipeline().lookup(agent, *r).tag().to_string())
            .collect();
        if tags.iter().all(|t| t != "pending_replay" && t != "pending") {
            return tags;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "replay must settle: {tags:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Scoped derives and actions deferred at close are durable `repo_scoped`
/// intents; the next attach replays each under its own repository key, an
/// unscoped one under the legacy key, and the store keeps only settled rows.
#[tokio::test]
async fn deferred_scoped_writes_replay_under_their_own_keys() {
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let mem = hybrid_on(
        &inner,
        "repo-replay",
        Arc::new(super::replay::HangingEmbedder::new()),
    )
    .build()
    .await
    .unwrap();
    let derive = |repo: Option<&'static str>| {
        let (mem, agent) = (&mem, &agent);
        async move {
            mem.derive_async_as(
                agent,
                &[("src/main.rs", ConceptType::Resource)],
                &ParentOf::none().with_repo_scope(repo),
                None,
            )
            .await
            .unwrap()
            .receipt
        }
    };
    let receipts = [
        derive(Some("repo-a")).await,
        derive(None).await,
        mem.record_action_async_as(
            &agent,
            &Action {
                repo_scope: Some("repo-b"),
                event_time: None,
                action: "built repo b",
                produces: &["src/main.rs"],
                modifies: &[],
                depends_on: &[],
            },
        )
        .await
        .unwrap()
        .receipt,
    ];
    mem.close().await.unwrap();
    assert_eq!(mem.pipeline().counters().deferred(), 3);
    let sid = SessionId::new("repo-replay");
    let pending = inner.load_session(&sid).await.unwrap().write_intents;
    let scoped = pending
        .iter()
        .filter(|i| {
            matches!(
                i.payload,
                crate::types::WriteIntentPayload::RepoScoped { .. }
            )
        })
        .count();
    assert_eq!(scoped, 2, "{pending:?}");

    let mem2 = hybrid_on(&inner, "repo-replay", Arc::new(FixtureEmbedder::new()))
        .build()
        .await
        .unwrap();
    let tags = settle_all(&mem2, &agent, &receipts).await;
    assert!(
        tags.iter().all(|t| t == "applied_after_restart"),
        "{tags:?}"
    );
    let a = path_id(&mem2, "repo-a", "src/main.rs");
    let b = path_id(&mem2, "repo-b", "src/main.rs");
    let legacy = mem2
        .graph
        .read()
        .concept_by_key(&crate::graph::canonical::canonical_key(
            "src/main.rs",
            |_| None,
        ))
        .expect("the unscoped derive keeps the legacy key");
    assert_eq!([a, b, legacy].into_iter().collect::<HashSet<_>>().len(), 3);
    mem2.close().await.unwrap();
    let settled = inner.load_session(&sid).await.unwrap().write_intents;
    assert!(
        settled.iter().all(|i| i.outcome.is_some()
            && !matches!(
                i.payload,
                crate::types::WriteIntentPayload::RepoScoped { .. }
            )),
        "{settled:?}"
    );
}

/// A stored `repo_scoped` row this build refuses (a nested wrapper) settles
/// `failed` with nothing written, and the replay goes on to the next row.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn a_malformed_scoped_intent_settles_failed_and_the_replay_goes_on() {
    use crate::types::{Interaction, WriteIntent, WriteIntentPayload};
    use std::str::FromStr;
    let inner = Arc::new(MemoryStore::new());
    let sid = SessionId::new("repo-poison");
    let agent = AgentId::new("agent-a");
    let interaction = NodeId::new();
    let now = Utc::now();
    let derive = WriteIntentPayload::Derive {
        concepts: vec![("src/main.rs".into(), ConceptType::Resource)],
        pairs: vec![],
    };
    let wrap = |repo: &str, payload: WriteIntentPayload| WriteIntentPayload::RepoScoped {
        repo_scope: repo.into(),
        payload: Box::new(payload),
    };
    let receipts = [
        "lwr1.00000000deadbee2.18f00000000.1",
        "lwr1.00000000deadbee2.18f00000000.2",
    ];
    let pending = |receipt: &str, seq: u64, payload: WriteIntentPayload| WriteIntent {
        session_id: sid.clone(),
        receipt: receipt.to_string(),
        agent: agent.clone(),
        interaction,
        lane_seq: seq,
        issued_ms: 1_755_000_000_000,
        payload,
        created_at: now,
        outcome: None,
    };
    inner
        .seed(GraphSnapshot {
            session_id: sid.clone(),
            embedding: Some(contract("fixture", 1024)),
            interactions: vec![Interaction {
                event_time: None,
                id: interaction,
                session_id: sid.clone(),
                agent_id: agent.clone(),
                prompt_text: None,
                previous_id: None,
                created_at: now,
            }],
            write_intents: vec![
                pending(receipts[0], 1, wrap("outer", wrap("inner", derive.clone()))),
                pending(receipts[1], 2, wrap("repo-a", derive)),
            ],
            ..GraphSnapshot::default()
        })
        .expect("seed");
    let mem = hybrid_on(&inner, "repo-poison", Arc::new(FixtureEmbedder::new()))
        .build()
        .await
        .unwrap();
    let ids = receipts.map(|r| crate::writeq::ReceiptId::from_str(r).unwrap());
    let tags = settle_all(&mem, &agent, &ids).await;
    assert_eq!(tags, ["failed", "applied_after_restart"]);
    assert_eq!(mem.graph.read().concepts().count(), 1);
    path_id(&mem, "repo-a", "src/main.rs");
    mem.close().await.unwrap();
}

/// Every text is the same vector, so any unmatched item is a perfect
/// semantic match for every embedded concept.
#[derive(Debug)]
struct ConstEmbedder;

#[async_trait]
impl Embedder for ConstEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let mut v = vec![0.0f32; 1024];
        v[7] = 1.0;
        Ok(v)
    }
}

/// Hybrid: a scoped path never merges into a vector neighbour (not the
/// legacy hub, not another repository's file), and no later text merges
/// into a scoped path, even at similarity 1.0.
#[tokio::test]
async fn scoped_paths_neither_merge_nor_are_merged_into_at_similarity_one() {
    let mem = Memory::builder()
        .session("repo-const")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        // Ranked in the holder's graph, so a merge sees unflushed vectors.
        .store(
            Arc::new(super::writes::VectorSearchStore::graph_ranked(Arc::new(
                MemoryStore::new(),
            ))) as Arc<dyn GraphStore>,
        )
        .embedder(Arc::new(ConstEmbedder) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .unwrap();
    let agent = AgentId::new("agent-a");
    let derive = |content: &'static str, kind, repo: Option<&'static str>| {
        let (mem, agent) = (&mem, &agent);
        async move {
            mem.derive_as(
                agent,
                &[(content, kind)],
                &ParentOf::none().with_repo_scope(repo),
            )
            .await
            .unwrap()
        }
    };
    let legacy = derive("src/main.rs", ConceptType::Resource, None)
        .await
        .created[0];
    for repo in ["repo-a", "repo-b"] {
        let out = derive("src/main.rs", ConceptType::Resource, Some(repo)).await;
        assert_eq!(out.created.len(), 1, "{repo}: {out:?}");
        assert!(out.semantic_merged.is_empty(), "{repo}: {out:?}");
    }
    let scoped = [
        path_id(&mem, "repo-a", "src/main.rs"),
        path_id(&mem, "repo-b", "src/main.rs"),
    ];
    // An unrelated text merges by vector, but only into the unscoped hub.
    let out = derive(
        "the main entry point",
        ConceptType::Resource,
        Some("repo-a"),
    )
    .await;
    assert_eq!(out.semantic_merged, [legacy], "{out:?}");
    assert!(scoped.iter().all(|s| !out.semantic_merged.contains(s)));
    mem.close().await.unwrap();
}

/// Three `README.md` concepts: the legacy one and one per repository,
/// written by synchronous actions (no embedder needed).
fn three_readmes(mem: &Memory, agent: &AgentId) -> [NodeId; 3] {
    for repo in [None, Some("repo-a"), Some("repo-b")] {
        mem.record_action_as(
            agent,
            &Action {
                repo_scope: repo,
                event_time: None,
                action: "wrote the readme",
                produces: &["README.md"],
                modifies: &[],
                depends_on: &[],
            },
        )
        .unwrap();
    }
    let legacy = mem
        .graph
        .read()
        .concept_by_key(&crate::graph::canonical::canonical_key("README.md", |_| {
            None
        }))
        .unwrap();
    [
        legacy,
        path_id(mem, "repo-a", "README.md"),
        path_id(mem, "repo-b", "README.md"),
    ]
}

/// #19(b) under #129: `against` path text resolves to the call's scoped
/// key, like `depends_on`, on a derive and on an action, under both
/// strategies; a scope with no such file is refused rather than falling
/// back to the legacy concept, and an unscoped call still names the legacy
/// one.
#[tokio::test]
async fn scoped_against_contradicts_only_the_scoped_path() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-against-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        let [legacy, a, b] = three_readmes(&mem, &agent);
        let against = |content: &str| ConceptCitations {
            content: content.into(),
            depends_on: Vec::new(),
            against: vec!["README.md".into()],
        };
        let contradicts = |from: NodeId, to: NodeId| {
            mem.graph
                .read()
                .edge_between(from, to, EdgeType::Contradicts)
                .is_some()
        };
        // A scoped derive contradicts repo-b's file only.
        let concepts = [("the readme is stale", ConceptType::Logic)];
        let out = mem
            .derive_cited_as(
                &agent,
                &concepts,
                &ParentOf::none().with_repo_scope(Some("repo-b")),
                &[against(concepts[0].0)],
            )
            .await
            .unwrap();
        assert_eq!(out.contradicted, 1, "{strategy:?}");
        let source = mem
            .graph
            .read()
            .concepts()
            .find(|c| c.content == concepts[0].0)
            .unwrap()
            .id;
        assert!(contradicts(source, b));
        assert!(!contradicts(source, a) && !contradicts(source, legacy));
        // A scoped action contradicts repo-a's file only.
        let action = Action {
            repo_scope: Some("repo-a"),
            event_time: None,
            action: "replaced the readme",
            produces: &[],
            modifies: &[],
            depends_on: &[],
        };
        let out = mem
            .record_action_against_embedded_as(&agent, &action, &["README.md".into()])
            .await
            .unwrap();
        assert!(contradicts(out.action_node, a));
        assert!(!contradicts(out.action_node, b) && !contradicts(out.action_node, legacy));
        // No such file in repo-c: refused, never the legacy concept.
        let before = mem.graph.read().snapshot();
        let concepts = [("repo c has no readme", ConceptType::Logic)];
        let err = mem
            .derive_cited_as(
                &agent,
                &concepts,
                &ParentOf::none().with_repo_scope(Some("repo-c")),
                &[against(concepts[0].0)],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LamboError::Reference(r) if r.kind == ReferenceRefusalKind::Missing));
        let action_c = Action {
            repo_scope: Some("repo-c"),
            ..action
        };
        assert!(mem
            .record_action_against_embedded_as(&agent, &action_c, &["README.md".into()])
            .await
            .is_err());
        assert_eq!(mem.graph.read().snapshot(), before);
        // Unscoped, the legacy concept.
        let out = mem
            .record_action_against_embedded_as(
                &agent,
                &Action {
                    repo_scope: None,
                    action: "questioned the old readme",
                    ..action
                },
                &["README.md".into()],
            )
            .await
            .unwrap();
        assert!(contradicts(out.action_node, legacy));
        assert!(!contradicts(out.action_node, a) && !contradicts(out.action_node, b));
        mem.close().await.unwrap();
    }
}

/// Scoped derives and actions with `against`, deferred at close, are
/// `repo_scoped` wrappers around `derive_cited` and `action_cited`; the
/// next attach replays them under their scope, writing each contradiction
/// to that repository's file, and the stored rows settle.
#[tokio::test]
async fn deferred_scoped_against_replays_under_its_scope() {
    use crate::types::WriteIntentPayload;
    let inner = Arc::new(MemoryStore::new());
    let agent = AgentId::new("agent-a");
    let mem = hybrid_on(
        &inner,
        "repo-against-replay",
        Arc::new(super::replay::HangingEmbedder::new()),
    )
    .build()
    .await
    .unwrap();
    let [legacy, a, b] = three_readmes(&mem, &agent);
    let concepts = [("the readme is stale", ConceptType::Logic)];
    let derive = mem
        .derive_cited_async_as(
            &agent,
            &concepts,
            &ParentOf::none().with_repo_scope(Some("repo-b")),
            &[ConceptCitations {
                content: concepts[0].0.into(),
                depends_on: Vec::new(),
                against: vec!["README.md".into()],
            }],
            None,
        )
        .await
        .unwrap()
        .receipt;
    let action = mem
        .record_action_against_async_as(
            &agent,
            &Action {
                repo_scope: Some("repo-a"),
                event_time: None,
                action: "replaced the readme",
                produces: &[],
                modifies: &[],
                depends_on: &[],
            },
            &["README.md".into()],
        )
        .await
        .unwrap()
        .receipt;
    mem.close().await.unwrap();
    assert_eq!(mem.pipeline().counters().deferred(), 2);
    let sid = SessionId::new("repo-against-replay");
    let kinds: Vec<&str> = inner
        .load_session(&sid)
        .await
        .unwrap()
        .write_intents
        .iter()
        .filter(|i| i.outcome.is_none())
        .map(|i| match &i.payload {
            WriteIntentPayload::RepoScoped { payload, .. } => match payload.as_ref() {
                WriteIntentPayload::DeriveCited { .. } => "scoped derive_cited",
                WriteIntentPayload::ActionCited { .. } => "scoped action_cited",
                _ => "scoped other",
            },
            _ => "unscoped",
        })
        .collect();
    let mut kinds = kinds;
    kinds.sort();
    assert_eq!(kinds, ["scoped action_cited", "scoped derive_cited"]);

    let mem2 = hybrid_on(
        &inner,
        "repo-against-replay",
        Arc::new(FixtureEmbedder::new()),
    )
    .build()
    .await
    .unwrap();
    let tags = settle_all(&mem2, &agent, &[derive, action]).await;
    assert!(
        tags.iter().all(|t| t == "applied_after_restart"),
        "{tags:?}"
    );
    let g = mem2.graph.read();
    let source = g
        .concepts()
        .find(|c| c.content == concepts[0].0)
        .unwrap()
        .id;
    let actor = g
        .concepts()
        .find(|c| c.content == "replaced the readme")
        .unwrap()
        .id;
    assert!(g.edge_between(source, b, EdgeType::Contradicts).is_some());
    assert!(g.edge_between(actor, a, EdgeType::Contradicts).is_some());
    for from in [source, actor] {
        assert!(g
            .edge_between(from, legacy, EdgeType::Contradicts)
            .is_none());
    }
    drop(g);
    mem2.close().await.unwrap();
    assert!(inner
        .load_session(&sid)
        .await
        .unwrap()
        .write_intents
        .iter()
        .all(
            |i| i.outcome.is_some() && !matches!(i.payload, WriteIntentPayload::RepoScoped { .. })
        ));
}
