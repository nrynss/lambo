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
    {
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
    }
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

/// Text lookups outside the write path never quietly pick one of several
/// same-content concepts when one is a repository-scoped path: retract
/// refuses the text as ambiguous (listing ids and repositories) and takes
/// a full node id; inspect's focus refuses it the same way. With a single
/// match, or no scoped concept at all, both resolve as before.
#[tokio::test]
async fn retract_and_inspect_refuse_ambiguous_scoped_text() {
    use crate::memory::DryRun;
    use crate::surface::focus::{ambiguous_refusal, resolve_focus, Focus};
    let mem = memory("repo-retract", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let [legacy, a, b] = three_readmes(&mem, &agent);
    let before = mem.graph.read().snapshot();
    let err = mem.retract("README.md", DryRun::No).await.unwrap_err();
    let LamboError::Reference(r) = &err else {
        panic!("{err:?}")
    };
    let ReferenceRefusalKind::Ambiguous { candidates } = &r.kind else {
        panic!("{r:?}")
    };
    let mut ids: Vec<NodeId> = candidates.iter().map(|c| c.node).collect();
    ids.sort_by_key(|n| n.0);
    let mut want = vec![legacy, a, b];
    want.sort_by_key(|n| n.0);
    assert_eq!(ids, want);
    let text = err.to_string();
    for needle in [
        "target \"README.md\" matches 3 concepts",
        "(repo \"repo-a\")",
        "(repo \"repo-b\")",
        "name one by its full node id",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert_eq!(mem.graph.read().snapshot(), before, "nothing retracted");
    // Inspect's focus refuses the same text, naming each repository.
    match resolve_focus(&mem.graph.read(), "README.md") {
        Focus::Ambiguous { candidates, .. } => {
            assert_eq!(candidates.len(), 3);
            let msg = ambiguous_refusal("README.md", &candidates, None);
            assert!(
                msg.contains("(repo \"repo-a\")") && msg.contains("(repo \"repo-b\")"),
                "{msg}"
            );
        }
        other => panic!("{other:?}"),
    }
    // A full id names exactly one; it retracts that one and no other.
    mem.retract(&b.0.to_string(), DryRun::No).await.unwrap();
    assert!(mem.graph.read().node(b).is_none());
    assert!(mem.graph.read().node(a).is_some() && mem.graph.read().node(legacy).is_some());
    mem.close().await.unwrap();

    // One scoped match alone, and no scoped concept at all: as before.
    let mem = memory("repo-retract-single", MatchStrategy::Canonical).await;
    mem.record_action_as(
        &agent,
        &Action {
            repo_scope: Some("repo-a"),
            event_time: None,
            action: "wrote the notes",
            produces: &["NOTES.md"],
            modifies: &[],
            depends_on: &[],
        },
    )
    .unwrap();
    let only = path_id(&mem, "repo-a", "NOTES.md");
    assert!(matches!(resolve_focus(&mem.graph.read(), "NOTES.md"), Focus::Exact(n) if n == only));
    mem.retract("NOTES.md", DryRun::No).await.unwrap();
    assert!(mem.graph.read().node(only).is_none());
    mem.close().await.unwrap();
}

/// Recall labels a repository-scoped hit with its repository, in the
/// context block and on the hit, so two repositories' `README.md` read
/// differently; an unscoped hit's label and serialized hit are unchanged.
#[tokio::test]
async fn recall_labels_scoped_hits_with_their_repository() {
    let mem = memory("repo-recall", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let [legacy, a, b] = three_readmes(&mem, &agent);
    let out = mem
        .recall(RecallQuery {
            query: "README.md".into(),
            top_k: 10,
            max_tokens: 2_000,
            traversal_depth: 1,
        })
        .await
        .unwrap();
    let hit = |id: NodeId| out.hits.iter().find(|h| h.node_id == id).expect("hit");
    assert_eq!(hit(a).repo_scope.as_deref(), Some("repo-a"));
    assert_eq!(hit(b).repo_scope.as_deref(), Some("repo-b"));
    assert_eq!(hit(legacy).repo_scope, None);
    assert!(
        out.context
            .contains("README.md [Resource, repo \"repo-a\"]"),
        "{}",
        out.context
    );
    assert!(
        out.context
            .contains("README.md [Resource, repo \"repo-b\"]"),
        "{}",
        out.context
    );
    assert!(
        out.context.contains("README.md [Resource] ("),
        "{}",
        out.context
    );
    let legacy_json = serde_json::to_string(hit(legacy)).unwrap();
    assert!(!legacy_json.contains("repo_scope"), "{legacy_json}");
    mem.close().await.unwrap();
}

/// Inspect's neighbourhood labels a scoped node with its repository, as
/// recall does, and leaves an unscoped node's label unchanged.
#[tokio::test]
async fn inspect_labels_scoped_nodes_with_their_repository() {
    use crate::surface::neighbourhood::render_neighbourhood_parts;
    let mem = memory("repo-inspect", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let [legacy, a, _] = three_readmes(&mem, &agent);
    let (scoped, plain) = {
        let g = mem.graph.read();
        (
            render_neighbourhood_parts(&g, a, 1).text(None),
            render_neighbourhood_parts(&g, legacy, 1).text(None),
        )
    };
    assert!(
        scoped.contains("README.md [Resource, repo \"repo-a\"]"),
        "{scoped}"
    );
    assert!(plain.contains("README.md [Resource]"), "{plain}");
    mem.close().await.unwrap();
}

fn legacy_id(mem: &Memory, text: &str) -> Option<NodeId> {
    mem.graph
        .read()
        .concept_by_key(&crate::graph::canonical::canonical_key(text, |_| None))
}

fn scoped_action<'a>(repo: &'a str, action: &'a str, depends_on: &'a [&'a str]) -> Action<'a> {
    Action {
        repo_scope: Some(repo),
        event_time: None,
        action,
        produces: &[],
        modifies: &[],
        depends_on,
    }
}

/// Review M1: under a repo scope, slash prose (`TCP/IP`, `CI/CD`) is not
/// inferred as a path. An action's `depends_on`, a citation and an
/// undeclared `parent_of` end name the existing non-Resource concept under
/// its legacy key (no duplicate is minted), and new prose stays legacy
/// text; an existing Logic `config.toml` stays citable by text.
#[tokio::test]
async fn scoped_inference_keeps_prose_and_non_resource_concepts_legacy() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-prose-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        let out = mem
            .derive_as(
                &agent,
                &[
                    ("config.toml", ConceptType::Logic),
                    ("TCP/IP", ConceptType::Entity),
                ],
                &ParentOf::none(),
            )
            .await
            .unwrap();
        let (config, tcp) = (out.created[0], out.created[1]);
        let before = mem.graph.read().concepts().count();
        // An action's depends_on: the existing concepts, plus new prose kept
        // legacy rather than minted as a scoped Resource.
        let out = mem
            .record_action_embedded_as(
                &agent,
                &scoped_action(
                    "repo-a",
                    "tuned the stack",
                    &["config.toml", "TCP/IP", "CI/CD"],
                ),
            )
            .await
            .unwrap();
        for target in [config, tcp] {
            assert!(mem
                .graph
                .read()
                .edge_between(out.action_node, target, EdgeType::Dependency)
                .is_some());
        }
        let cicd = legacy_id(&mem, "CI/CD").expect("CI/CD stays legacy text");
        assert!(scoped_resource_key("CI/CD", Some("repo-a"))
            .and_then(|k| mem.graph.read().concept_by_key(&k))
            .is_none());
        // Only the action and CI/CD are new: no duplicate config.toml or TCP/IP.
        assert_eq!(mem.graph.read().concepts().count(), before + 2);
        // A citation by text reaches the Logic config.toml under a scope.
        let concepts = [("deployment verified", ConceptType::Logic)];
        let out = mem
            .derive_cited_as(
                &agent,
                &concepts,
                &ParentOf::none().with_repo_scope(Some("repo-a")),
                &[ConceptCitations {
                    content: concepts[0].0.into(),
                    depends_on: vec!["config.toml".into(), "TCP/IP".into()],
                    against: Vec::new(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(out.cited, 2, "{strategy:?}");
        // An undeclared parent_of end: the legacy Entity, not a scoped path.
        let pairs = [("networking", "TCP/IP")];
        mem.derive_as(
            &agent,
            &[("networking", ConceptType::Entity)],
            &ParentOf::from_pairs(&pairs).with_repo_scope(Some("repo-a")),
        )
        .await
        .unwrap();
        let net = legacy_id(&mem, "networking").unwrap();
        assert!(mem
            .graph
            .read()
            .edge_between(net, tcp, EdgeType::Hierarchical)
            .is_some());
        let _ = cicd;
        mem.close().await.unwrap();
    }
}

/// Review M1: text that names both an existing scoped path and an existing
/// non-Resource concept is refused as ambiguous, on a citation, an action's
/// depends_on and an undeclared parent_of end, writing nothing.
#[tokio::test]
async fn scoped_text_naming_both_identities_is_ambiguous() {
    let mem = memory("repo-both", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    mem.derive_as(
        &agent,
        &[("config.toml", ConceptType::Logic)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    mem.derive_as(
        &agent,
        &[("config.toml", ConceptType::Resource)],
        &ParentOf::none().with_repo_scope(Some("repo-a")),
    )
    .await
    .unwrap();
    // Concepts and their edges; a refused synchronous write may still have
    // opened its interaction, as for any graph-level refusal.
    let written = |mem: &Memory| {
        let snap = mem.graph.read().snapshot();
        let concepts: HashSet<NodeId> = snap.concepts.iter().map(|c| c.id).collect();
        let edges: Vec<(NodeId, NodeId)> = snap
            .edges
            .iter()
            .filter(|e| concepts.contains(&e.source) && concepts.contains(&e.target))
            .map(|e| (e.source, e.target))
            .collect();
        (concepts, edges)
    };
    let before = written(&mem);
    let ambiguous = |e: LamboError| {
        matches!(&e, LamboError::Reference(r)
            if matches!(r.kind, ReferenceRefusalKind::Ambiguous { .. }))
    };
    let concepts = [("the config is wrong", ConceptType::Logic)];
    let err = mem
        .derive_cited_as(
            &agent,
            &concepts,
            &ParentOf::none().with_repo_scope(Some("repo-a")),
            &[ConceptCitations {
                content: concepts[0].0.into(),
                depends_on: vec!["config.toml".into()],
                against: Vec::new(),
            }],
        )
        .await
        .unwrap_err();
    assert!(ambiguous(err));
    let err = mem
        .record_action_as(
            &agent,
            &scoped_action("repo-a", "read the config", &["config.toml"]),
        )
        .unwrap_err();
    assert!(ambiguous(err));
    let pairs = [("settings", "config.toml")];
    let err = mem
        .derive_as(
            &agent,
            &[("settings", ConceptType::Entity)],
            &ParentOf::from_pairs(&pairs).with_repo_scope(Some("repo-a")),
        )
        .await
        .unwrap_err();
    assert!(ambiguous(err));
    assert_eq!(written(&mem), before);
    mem.close().await.unwrap();
}

/// Review M3: a lone directory parent of a scoped path is that
/// repository's directory, not a legacy Entity two repositories share;
/// `src/` and `src` name one directory.
#[tokio::test]
async fn scoped_directory_parents_stay_per_repository() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-dirs-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        for (repo, parent) in [("repo-a", "src"), ("repo-b", "src"), ("repo-a", "src/")] {
            let pairs = [(parent, "src/main.rs")];
            mem.derive_as(
                &agent,
                &[("layout recorded", ConceptType::Logic)],
                &ParentOf::from_pairs(&pairs).with_repo_scope(Some(repo)),
            )
            .await
            .unwrap();
        }
        let dir = |repo| path_id_unchecked(&mem, repo, "src");
        let (a, b) = (dir("repo-a"), dir("repo-b"));
        assert_ne!(a, b);
        assert!(legacy_id(&mem, "src").is_none(), "no shared legacy src");
        for (repo, d) in [("repo-a", a), ("repo-b", b)] {
            let child = path_id(&mem, repo, "src/main.rs");
            assert!(mem
                .graph
                .read()
                .edge_between(d, child, EdgeType::Hierarchical)
                .is_some());
        }
        mem.close().await.unwrap();
    }
}

fn path_id_unchecked(mem: &Memory, repo: &str, path: &str) -> NodeId {
    mem.graph
        .read()
        .concept_by_key(&crate::graph::resource::scoped_key_unchecked(path, repo))
        .unwrap()
}

/// Review L9: repo A's `a.md` over `b.md` and repo B's `b.md` over `a.md`
/// are four files, not a Hierarchical cycle.
#[tokio::test]
async fn opposite_hierarchies_in_two_repositories_are_not_a_cycle() {
    let mem = memory("repo-no-cycle", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    for (repo, pair) in [("repo-a", ("a.md", "b.md")), ("repo-b", ("b.md", "a.md"))] {
        let pairs = [pair];
        mem.derive_as(
            &agent,
            &[("docs nested", ConceptType::Logic)],
            &ParentOf::from_pairs(&pairs).with_repo_scope(Some(repo)),
        )
        .await
        .unwrap();
    }
    // The same two pairs unscoped do close a cycle.
    mem.derive_as(
        &agent,
        &[("docs nested", ConceptType::Logic)],
        &ParentOf::from_pairs(&[("a.md", "b.md")]),
    )
    .await
    .unwrap();
    let err = mem
        .derive_as(
            &agent,
            &[("docs nested", ConceptType::Logic)],
            &ParentOf::from_pairs(&[("b.md", "a.md")]),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
    mem.close().await.unwrap();
}

/// Review M2: layered recall dedupes a scoped path only by an equal key.
/// With every vector identical, project repo-a's `README.md` absorbs the
/// team layer's repo-a copy (same key), but neither the general layer's
/// repo-b `README.md` nor its unscoped prose joins it by vector.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layered_dedupe_joins_scoped_paths_only_by_key() {
    use crate::memory::LayerRef;
    async fn session(name: &str) -> Memory {
        Memory::builder()
            .session(name)
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
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
            .unwrap()
    }
    let (project, team, general) = (
        session("repo-layer-p").await,
        session("repo-layer-t").await,
        session("repo-layer-g").await,
    );
    let agent = AgentId::new("agent-a");
    for (mem, repo) in [
        (&project, "repo-a"),
        (&team, "repo-a"),
        (&general, "repo-b"),
    ] {
        mem.derive_as(
            &agent,
            &[("README.md", ConceptType::Resource)],
            &ParentOf::none().with_repo_scope(Some(repo)),
        )
        .await
        .unwrap();
    }
    general
        .derive_as(
            &agent,
            &[("release notes", ConceptType::Logic)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    let r = project
        .recall_layered(
            &[LayerRef::new(&team), LayerRef::new(&general)],
            RecallQuery {
                query: "README.md".into(),
                top_k: 10,
                max_tokens: 2_000,
                traversal_depth: 1,
            },
        )
        .await
        .unwrap();
    let shown: Vec<(String, Option<String>, usize)> = r
        .hits
        .iter()
        .map(|h| {
            (
                h.session.to_string(),
                h.hit.repo_scope.clone(),
                h.also.len(),
            )
        })
        .collect();
    let a = r
        .hits
        .iter()
        .find(|h| h.hit.repo_scope.as_deref() == Some("repo-a"))
        .expect("repo-a README.md");
    assert_eq!(a.session.to_string(), "repo-layer-p", "{shown:?}");
    assert_eq!(
        a.also.len(),
        1,
        "team's repo-a copy joins by key: {shown:?}"
    );
    let b = r
        .hits
        .iter()
        .find(|h| h.hit.repo_scope.as_deref() == Some("repo-b"))
        .expect("repo-b README.md is its own hit");
    assert!(b.also.is_empty(), "{shown:?}");
    assert!(
        r.hits.iter().any(|h| h.hit.content == "release notes"),
        "unscoped prose is not merged into a scoped path: {shown:?}"
    );
    for m in [project, team, general] {
        m.close().await.unwrap();
    }
}

/// Focused review L1: a trailing line reference names the file, so
/// `src/main.rs`, `src/main.rs:12` and `src/main.rs#L3-L5` are one scoped
/// concept (its content keeps its first write's spelling).
#[tokio::test]
async fn line_references_fold_to_one_scoped_file() {
    let mem = memory("repo-lines", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let mut ids = Vec::new();
    for entry in ["src/main.rs:12", "src/main.rs", "src/main.rs#L3-L5"] {
        let out = mem
            .record_action_as(
                &agent,
                &Action {
                    repo_scope: Some("repo-a"),
                    event_time: None,
                    action: "edited main",
                    produces: &[],
                    modifies: &[entry],
                    depends_on: &[],
                },
            )
            .unwrap();
        let _ = out;
        ids.push(path_id(&mem, "repo-a", entry));
    }
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    let content = mem
        .graph
        .read()
        .concepts()
        .find(|c| c.id == ids[0])
        .unwrap()
        .content
        .clone();
    assert_eq!(content, "src/main.rs:12");
    mem.close().await.unwrap();
}

/// Focused review L2, documented rather than changed: a loosely
/// path-like name (`scripts/build`) that an inferred `depends_on` meets
/// first stays plain text, and a later explicit `produces` is a separate
/// scoped path; a scoped `produces` never joins a plain concept another
/// repository may share. Spelled as a clear path (`./tools/gen`), both
/// orders name the one scoped concept.
#[tokio::test]
async fn loose_and_clear_path_spellings_under_depends_on_then_produces() {
    let mem = memory("repo-order", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    // Distinct action texts, so each action is its own node.
    let act = |repo,
               action: &'static str,
               depends_on: &'static [&'static str],
               produces: &'static [&'static str]| Action {
        repo_scope: Some(repo),
        event_time: None,
        action,
        produces,
        modifies: &[],
        depends_on,
    };
    let edge_to = |out: &crate::graph::action::ActionOutcome, content: &str, ty| {
        let g = mem.graph.read();
        g.concepts()
            .filter(|c| c.content == content)
            .map(|c| c.id)
            .find(|&id| g.edge_between(out.action_node, id, ty).is_some())
            .expect("edge")
    };
    // Loose spelling: depends_on first is plain, produces later is scoped.
    let a = mem
        .record_action_as(
            &agent,
            &act("repo-a", "checked step one", &["scripts/build"], &[]),
        )
        .unwrap();
    let b = mem
        .record_action_as(
            &agent,
            &act("repo-a", "checked step two", &[], &["scripts/build"]),
        )
        .unwrap();
    let plain = edge_to(&a, "scripts/build", EdgeType::Dependency);
    let scoped = edge_to(&b, "scripts/build", EdgeType::Causal);
    assert_ne!(plain, scoped);
    assert_eq!(scoped, path_id(&mem, "repo-a", "scripts/build"));
    // Another repository's produces does not join the plain concept.
    let c = mem
        .record_action_as(
            &agent,
            &act("repo-b", "checked step three", &[], &["scripts/build"]),
        )
        .unwrap();
    assert_eq!(
        edge_to(&c, "scripts/build", EdgeType::Causal),
        path_id(&mem, "repo-b", "scripts/build")
    );
    // Clear spelling: one scoped concept in either order.
    let d = mem
        .record_action_as(
            &agent,
            &act("repo-c", "checked step four", &["./tools/gen"], &[]),
        )
        .unwrap();
    let e = mem
        .record_action_as(
            &agent,
            &act("repo-c", "checked step five", &[], &["./tools/gen"]),
        )
        .unwrap();
    assert_eq!(
        edge_to(&d, "./tools/gen", EdgeType::Dependency),
        edge_to(&e, "./tools/gen", EdgeType::Causal)
    );
    mem.close().await.unwrap();
}
