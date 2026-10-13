//! Generic multi-repository identity tests, with no rig data.
use super::*;
use crate::graph::resource::scoped_resource_key;
use crate::test_util::VectorSearchable;
use crate::types::{ConceptCitations, EdgeType, ReferenceRefusalKind};
use crate::writeq::RECEIPT_WAIT_MAX;

async fn memory(session: &str, strategy: MatchStrategy) -> Memory {
    Memory::builder().session(session).agent("agent-a")
        .store(Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))) as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(strategy).flush_interval(Duration::from_secs(3600))
        .build().await.unwrap()
}

fn path_id(mem: &Memory, repo: &str, path: &str) -> NodeId {
    mem.graph.read().concept_by_key(&scoped_resource_key(path, Some(repo)).unwrap()).unwrap()
}

#[tokio::test]
async fn repo_scope_isolates_paths_and_keeps_legacy_and_nonresources() {
    for strategy in [MatchStrategy::Canonical, MatchStrategy::Hybrid] {
        let mem = memory(&format!("repo-identity-{strategy:?}"), strategy).await;
        let agent = AgentId::new("agent-a");
        mem.derive_as(&agent, &[("src/main.rs", ConceptType::Resource)], &ParentOf::none()).await.unwrap();
        for repo in ["repo-a", "repo-b"] {
            let context = ParentOf::none().with_repo_scope(Some(repo));
            let out = mem.derive_as(&agent, &[
                ("src/main.rs", ConceptType::Resource),
                ("src/Main.rs", ConceptType::Resource),
                ("docs/reference", ConceptType::Resource),
                ("src/a_b.rs", ConceptType::Resource),
                ("src/b_a.rs", ConceptType::Resource),
            ], &context).await.unwrap();
            assert_eq!(out.created.len(), 5);
            assert!(out.semantic_merged.is_empty());
            let repeat = mem.derive_as(&agent, &[("src/main.rs", ConceptType::Resource)], &context).await.unwrap();
            assert_eq!(repeat.matched, [path_id(&mem, repo, "src/main.rs")]);
            assert!(repeat.created.is_empty());
        }
        assert_ne!(path_id(&mem, "repo-a", "src/main.rs"), path_id(&mem, "repo-b", "src/main.rs"));
        assert_ne!(path_id(&mem, "repo-a", "src/main.rs"), path_id(&mem, "repo-a", "src/Main.rs"));
        assert_eq!(mem.graph.read().concepts().count(), 11);
        // Explicit non-Resources retain the legacy canonical key.
        let out = mem.derive_as(&agent, &[("config.toml", ConceptType::Logic)],
            &ParentOf::none().with_repo_scope(Some("repo-a"))).await.unwrap();
        let logic = out.created[0];
        assert_eq!(mem.graph.read().concept_by_key(&crate::graph::canonical::canonical_key("config.toml", |_| None)), Some(logic));
        assert!(scoped_resource_key("a decision", Some("repo-a")).is_none());
        mem.close().await.unwrap();
    }
}

#[tokio::test]
async fn queued_actions_and_derives_share_scoped_file_dependencies() {
    let mem = memory("repo-queued-actions", MatchStrategy::Hybrid).await;
    let agent = AgentId::new("agent-a");
    for repo in ["repo-a", "repo-b"] {
        let action = Action { repo_scope: Some(repo), event_time: None,
            action: "built the binary", produces: &["src/main.rs"], modifies: &[], depends_on: &[] };
        let submitted = mem.record_action_async_as(&agent, &action).await.unwrap();
        let answer = mem.pipeline().wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX).await;
        assert!(answer.is_settled(), "{}", answer.describe());
        assert_ne!(answer.tag(), "failed", "{}", answer.describe());
    }
    let a = path_id(&mem, "repo-a", "src/main.rs");
    let b = path_id(&mem, "repo-b", "src/main.rs");
    assert_ne!(a, b);
    let action = Action { repo_scope: Some("repo-b"), event_time: None,
        action: "checked the artifact", produces: &[], modifies: &[], depends_on: &["src/main.rs"] };
    let out = mem.record_action_embedded_as(&agent, &action).await.unwrap();
    assert!(mem.graph.read().edge_between(out.action_node, b, EdgeType::Dependency).is_some());
    assert!(mem.graph.read().edge_between(out.action_node, a, EdgeType::Dependency).is_none());
    let citations = [ConceptCitations { content: "artifact has been checked".into(), depends_on: vec!["src/main.rs".into()] }];
    let submitted = mem.derive_cited_async_as(&agent, &[("artifact has been checked", ConceptType::Observation)],
        &ParentOf::none().with_repo_scope(Some("repo-a")), &citations, None).await.unwrap();
    let answer = mem.pipeline().wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX).await;
    assert_ne!(answer.tag(), "failed", "{}", answer.describe());
    let source = mem.graph.read().concepts().find(|c| c.content == "artifact has been checked").unwrap().id;
    assert!(mem.graph.read().edge_between(source, a, EdgeType::Dependency).is_some());
    mem.close().await.unwrap();
}

#[tokio::test]
async fn scoped_citations_refuse_legacy_fallback_but_allow_explicit_ids() {
    let mem = memory("repo-citation-fallback", MatchStrategy::Canonical).await;
    let agent = AgentId::new("agent-a");
    let old = mem.derive_as(&agent, &[("README.md", ConceptType::Resource)], &ParentOf::none()).await.unwrap().created[0];
    let context = ParentOf::none().with_repo_scope(Some("repo-a"));
    let concepts = [("documented the build", ConceptType::Logic)];
    let citations = [ConceptCitations { content: concepts[0].0.into(), depends_on: vec!["README.md".into()] }];
    let before = mem.graph.read().snapshot();
    let err = mem.derive_cited_as(&agent, &concepts, &context, &citations).await.unwrap_err();
    assert!(matches!(err, LamboError::Reference(r) if r.kind == ReferenceRefusalKind::Missing));
    assert_eq!(mem.graph.read().snapshot(), before);
    let explicit = [ConceptCitations { content: concepts[0].0.into(), depends_on: vec![old.to_string()] }];
    let out = mem.derive_cited_as(&agent, &concepts, &context, &explicit).await.unwrap();
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
            mem.derive_as(&agent, &[("hierarchy complete", ConceptType::Logic)], &context).await.unwrap();
            let parent = path_id(&mem, repo, pairs[0].0);
            let child = path_id(&mem, repo, pairs[0].1);
            assert!(mem.graph.read().edge_between(parent, child, EdgeType::Hierarchical).is_some());
        }
        let before = mem.graph.read().snapshot();
        assert!(mem.derive_async_as(&agent, &[("a.rs", ConceptType::Resource)],
            &ParentOf::none().with_repo_scope(Some("  ")), None).await.is_err());
        assert_eq!(mem.graph.read().snapshot(), before);
        mem.close().await.unwrap();
    }
}
