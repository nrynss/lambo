//! /api/graph: the structural skeleton and its bounds.

use super::*;

/// A session of `n` plain concepts (and one interaction to root them),
/// with no canonization — for driving `/api/graph` past its node bound.
async fn seed_many_concepts(session: &str, n: usize) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new(session);
    let iid = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("root".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    for i in 0..n {
        let cid = NodeId::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(
                sid.clone(),
                cid,
                iid,
                &format!("concept {i:05}"),
                now,
            )),
        });
        // §5.7: every concept must have a Derives edge from an interaction.
        batch.push(Mutation::UpsertEdge {
            edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
        });
    }
    store.flush(&batch, None).await.expect("seed many");
    store
}

/// A session of `concepts` plain concepts with a Dependency edge between
/// *every increasing* pair (`source_i -> source_j` for `i < j`) — exactly
/// `concepts * (concepts - 1) / 2` structural edges, all acyclic — for
/// driving `/api/graph` past its edge bound without crossing the node
/// bound. The `i < j` ordering keeps the structural edges a DAG (the graph
/// builder rejects Dependency cycles), and every natural key
/// (source, target, Dependency) is distinct, so MemoryStore (which dedupes
/// edges by that triple) keeps them all.
async fn seed_many_structural_edges(session: &str, concepts: usize) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new(session);
    let iid = NodeId::new();
    let now = Utc::now();
    let ids: Vec<NodeId> = (0..concepts).map(|_| NodeId::new()).collect();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("root".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    for (i, &cid) in ids.iter().enumerate() {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(
                sid.clone(),
                cid,
                iid,
                &format!("concept {i:05}"),
                now,
            )),
        });
        // §5.7: every concept must have a Derives edge from an interaction.
        batch.push(Mutation::UpsertEdge {
            edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
        });
    }
    for (i, &src) in ids.iter().enumerate() {
        for &dst in ids.iter().skip(i + 1) {
            batch.push(Mutation::UpsertEdge {
                edge: edge(
                    NodeId::new(),
                    sid.clone(),
                    src,
                    dst,
                    EdgeType::Dependency,
                    now,
                ),
            });
        }
    }
    store
        .flush(&batch, None)
        .await
        .expect("seed many structural edges");
    store
}

// ---- /api/graph -----------------------------------------------------

/// The tree view ships the structural skeleton: concepts with their row
/// status + blast radius, and structural edges only.
#[tokio::test]
async fn graph_endpoint_returns_the_structural_skeleton() {
    let store = seed("t93-graph").await;
    let (addr, handle) = spawn(state_on(store, "t93-graph")).await;

    let g = get_json(addr, "/api/graph").await;
    assert_eq!(g["session"], "t93-graph", "{g}");
    assert_eq!(g["truncated"], false, "{g}");

    let nodes = g["nodes"].as_array().expect("nodes");
    assert!(!nodes.is_empty(), "{g}");
    for n in nodes {
        assert!(n["content"].as_str().is_some(), "{g}");
        assert!(n["concept_type"].as_str().is_some(), "{g}");
        assert!(n["status"].as_str().is_some(), "{g}");
        assert!(n["blast_radius"].as_i64().is_some(), "{g}");
    }
    assert!(
        nodes
            .iter()
            .any(|n| n["content"] == "user schema" && n["status"] == "Canonical"),
        "the canonical concept with its row status must be a node: {g}"
    );

    let edges = g["edges"].as_array().expect("edges");
    for e in edges {
        let ty = e["edge"].as_str().expect("edge");
        assert!(
            matches!(ty, "Dependency" | "Causal" | "Hierarchical"),
            "non-structural edge '{ty}' in the tree: {g}"
        );
        assert!(e["parent"].as_str().is_some(), "{g}");
        assert!(e["child"].as_str().is_some(), "{g}");
    }

    handle.abort();
}

/// A non-canonical (here status `None`) node with dependents must report a
/// nonzero `blast_radius` on BOTH `/api/graph` and `/api/inspect` — the
/// live dependent count from the same helper, not the frozen
/// concepts-row column (which is `None` until promotion), so the tree
/// foregrounds load-bearing Candidates/Venerables (T3-R1-2).
#[tokio::test]
async fn graph_and_inspect_agree_on_a_live_blast_radius_for_non_canonical() {
    // The seed's `create user` action is never promoted (status `None`)
    // but stands behind a dependent, so format::blast_radii counts it.
    let store = seed("t93-graph-live").await;
    let (addr, handle) = spawn(state_on(store, "t93-graph-live")).await;

    let g = get_json(addr, "/api/graph").await;
    let pillar = g["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["content"] == "create user")
        .expect("create user node");
    assert_ne!(pillar["status"], "Canonical", "{g}");
    let graph_radius = pillar["blast_radius"].as_u64().expect("radius");
    assert!(
        graph_radius >= 1,
        "a non-canonical node with a dependent must report a nonzero live radius: {g}"
    );

    let hit = get_json(addr, "/api/inspect?focus=create%20user").await;
    assert_eq!(hit["found"], true, "{hit}");
    assert_eq!(
        hit["blast_radius"].as_u64().unwrap(),
        graph_radius,
        "/api/inspect and /api/graph must report the same live blast radius: {hit} {g}"
    );
    handle.abort();
}

/// Drive `/api/graph` past `MAX_GRAPH_NODES` concepts: `truncated` is true
/// and the payload pins at the cap (T3-R1-5).
#[tokio::test]
async fn graph_truncates_and_reports_at_the_nodes_bound() {
    let store = seed_many_concepts("t93-graph-cap", MAX_GRAPH_NODES + 1).await;
    let (addr, handle) = spawn(state_on(store, "t93-graph-cap")).await;

    let g = get_json(addr, "/api/graph").await;
    assert_eq!(g["truncated"], true, "{g}");
    assert_eq!(
        g["nodes"].as_array().expect("nodes").len(),
        MAX_GRAPH_NODES,
        "at the bound the payload must be pinned at the cap: {g}"
    );
    handle.abort();
}

/// Drive `/api/graph` past `MAX_GRAPH_EDGES` structural edges: `truncated`
/// is true and the payload pins at the edge cap, while the node count
/// stays under its own bound so the edge branch fires in isolation.
#[tokio::test]
async fn graph_truncates_and_reports_at_the_edges_bound() {
    // 182 concepts => 182 * 181 / 2 = 16471 Dependency edges >
    // MAX_GRAPH_EDGES (16384), but 182 concepts < MAX_GRAPH_NODES (4096)
    // keeps the node side untruncated. The i < j ordering keeps the
    // structural edges a DAG (the graph builder rejects cycles).
    let concepts = 182;
    let store = seed_many_structural_edges("t93-graph-edge-cap", concepts).await;
    let (addr, handle) = spawn(state_on(store, "t93-graph-edge-cap")).await;

    let g = get_json(addr, "/api/graph").await;
    assert_eq!(g["truncated"], true, "{g}");
    assert_eq!(
        g["edges"].as_array().expect("edges").len(),
        MAX_GRAPH_EDGES,
        "at the edge bound the payload must be pinned at the cap: {g}"
    );
    assert_eq!(
        g["nodes"].as_array().expect("nodes").len(),
        concepts,
        "nodes stay under their own bound and must not be cut: {g}"
    );
    handle.abort();
}

#[tokio::test]
async fn an_unwritten_session_is_an_empty_window_not_an_error() {
    let store = Arc::new(MemoryStore::new());
    let (addr, handle) = spawn(state_on(store, "t85-never-written")).await;

    let pulse = get_json(addr, "/api/pulse").await;
    assert_eq!(pulse["stats"]["nodes"], 0, "{pulse}");
    assert_eq!(pulse["events"]["total"], 0, "{pulse}");

    handle.abort();
}
