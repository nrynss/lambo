//! Ignored operator run against a frozen private snapshot; never a live writer.
use super::propagate;
use crate::daemon::Daemon;
use crate::graph::vector_source::{graph_vector_candidates, GraphVectorSource};
use crate::graph::{index::InvertedIndex, Graph};
use crate::recall::{assemble, candidates, expand};
use crate::store::vector_source::VectorCandidates;
use crate::store::GraphStore;
use crate::types::{Node, RecallQuery, SessionId};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn write_new(path: &Path, value: &Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    assert!(
        path.is_absolute(),
        "raw output needs an absolute private path"
    );
    let parent = path.parent().unwrap().canonicalize().unwrap();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    assert!(
        !parent.starts_with(repository),
        "raw output belongs outside Git"
    );
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(&mut f, value).unwrap();
    f.write_all(b"\n").unwrap();
}

/// Run explicitly with #175 input/config/output environment variables, single test.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "operator-only #175 frozen-snapshot experiment; requires private inputs and real embedder"]
async fn frozen_graph_experiment() {
    let database = PathBuf::from(env("LAMBO175_DATABASE"));
    let config = PathBuf::from(env("LAMBO175_CONFIG"));
    let input = PathBuf::from(env("LAMBO175_QUERIES"));
    let output = PathBuf::from(env("LAMBO175_OUTPUT"));
    let mode = env("LAMBO175_MODE");
    assert!(matches!(mode.as_str(), "pool" | "eval"));
    let dataset = read(&input);
    use sha2::{Digest, Sha256};
    let fingerprint = |path: &Path| format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()));
    let snapshot_sha256 = fingerprint(&database);
    assert_eq!(
        dataset["snapshot_sha256"], snapshot_sha256,
        "wrong frozen snapshot"
    );
    let dataset_sha256 = fingerprint(&input);
    let now: DateTime<Utc> = dataset["clock"].as_str().unwrap().parse().unwrap();
    let io = crate::resolve::resolve_verify_io(&config, Some(&database), None, &database).unwrap();
    assert_eq!(
        io.embedding.kind, "candle",
        "real benchmark requires the frozen Candle contract"
    );
    let store = io.split.store();
    store.preflight_schema().await.unwrap();
    let session = SessionId::from("lambo");
    let snapshot = store.load_session(&session).await.unwrap();
    crate::resolve::assert_session_embedding_compatible(snapshot.embedding.as_ref(), &io.embedding)
        .unwrap();
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snapshot)));
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snapshot).unwrap()));
    let before = serde_json::to_vec(&graph.read().snapshot()).unwrap();
    let daemon = Daemon::from_config(graph.clone(), &io.config)
        .with_index(index.clone())
        .with_clock(Arc::new(move || now));
    daemon.prime_offline();
    let scores = daemon.scores();
    let cache = crate::recall::cache::SharedRecallCache::with_capacity(128);
    let cache_path = PathBuf::from(env("LAMBO175_VECTORS"));
    let mut vectors = if cache_path.exists() {
        read(&cache_path)
    } else {
        json!({"contract":io.embedding,"queries":{}})
    };
    assert_eq!(
        vectors["contract"],
        serde_json::to_value(&io.embedding).unwrap()
    );
    let mut rows = Vec::new();
    for item in dataset["queries"].as_array().unwrap() {
        let qid = item["id"].as_str().unwrap();
        let text = item["query"].as_str().unwrap();
        let vector: Vec<f32> = if let Some(entry) = vectors["queries"].get(qid) {
            assert_eq!(entry["text"], text, "query ID changed after embedding");
            serde_json::from_value(entry["vector"].clone()).unwrap()
        } else {
            let v = io.embedder.embed_query(text).await.unwrap();
            vectors["queries"][qid] = json!({"text":text,"vector":v});
            v
        };
        let query = RecallQuery {
            query: text.into(),
            top_k: 10,
            max_tokens: 1000,
            traversal_depth: 2,
        };
        let production = daemon
            .recall_with(
                &session,
                query.clone(),
                VectorCandidates::Graph(GraphVectorSource::new(&graph)),
                Some((&vector, &io.embedding)),
                io.config.recall_weights,
                &cache,
            )
            .await;
        let (phase1, legs, expanded, payloads) = {
            let g = graph.read();
            let vector_hits =
                graph_vector_candidates(&g, &session, &vector, &io.embedding, query.top_k).unwrap();
            let input = candidates::Phase1Input {
                vector: vector_hits,
                recent_score: crate::embed::calibration::recent_score(
                    io.config.calibration,
                    Some(&io.embedding),
                ),
                fusion: io.config.recall_fusion,
            };
            let (phase1, legs) =
                candidates::candidates_with_legs(&g, &index.read(), input, text, query.top_k);
            let expanded = expand::expand(&g, phase1.clone(), query.traversal_depth);
            let payloads = daemon.hot_list().write().revalidate_members(
                &g,
                expanded
                    .required
                    .iter()
                    .chain(expanded.siblings.iter())
                    .map(|s| s.item),
                now,
            );
            (phase1, legs, expanded, payloads)
        };
        let g = graph.read();
        let baseline = assemble::assemble_with_legs(
            &g,
            &expanded,
            &phase1,
            Some(&legs),
            &scores,
            &payloads,
            &query,
            io.config.recall_weights,
            io.config.canonization_edge_min_age,
            crate::recall::suspected::shows_line(
                io.config.suspected_supersession_line,
                g.embedding(),
            ),
            None,
            now,
            assemble::default_token_count,
        );
        assert_eq!(
            baseline.hits, production.hits,
            "manual frame must equal actual production recall"
        );
        assert_eq!(baseline.context, production.context);
        assert_eq!(baseline.warnings, production.warnings);
        let direct: HashSet<_> = phase1.iter().map(|s| s.item).collect();
        let mut candidate_ids: Vec<_> = expanded
            .required
            .iter()
            .chain(expanded.siblings.iter())
            .map(|s| s.item)
            .collect();
        candidate_ids.extend(baseline.hits.iter().map(|h| h.node_id));
        candidate_ids.sort_by_key(|n| n.0);
        candidate_ids.dedup();
        let candidates: Vec<_> = candidate_ids
            .iter()
            .filter_map(|node| match g.node(*node) {
                Some(Node::Concept(c)) => Some(json!({"id":node,"text":c.content})),
                _ => None,
            })
            .collect();
        if mode == "pool" {
            rows.push(json!({"id":qid,"query":text,"candidates":candidates,
                "phase1":phase1,"bfs_ids":expanded.required.iter().map(|s|s.item).collect::<Vec<_>>(),
                "daemon_scores":scores.ranked.iter().filter(|s|candidate_ids.contains(&s.item)).collect::<Vec<_>>() }));
            continue;
        }
        let mut variants = Vec::new();
        let settings = dataset["settings"].as_array().unwrap();
        for setting in settings {
            let arm = setting["arm"].as_str().unwrap();
            assert!(matches!(arm, "zero" | "damped" | "degree"), "unknown arm");
            let damping = setting["damping"].as_f64().unwrap();
            let gain = setting["gain"].as_f64().unwrap();
            let started = Instant::now();
            let relevance = if arm == "zero" {
                HashMap::new()
            } else {
                propagate(&g, &phase1, &expanded, damping, gain, arm == "degree")
            };
            let result = assemble::assemble_with_graph_relevance(
                &g,
                &expanded,
                &phase1,
                Some(&legs),
                Some(&relevance),
                &scores,
                &payloads,
                &query,
                io.config.recall_weights,
                io.config.canonization_edge_min_age,
                crate::recall::suspected::shows_line(
                    io.config.suspected_supersession_line,
                    g.embedding(),
                ),
                None,
                now,
                assemble::default_token_count,
            );
            let micros = started.elapsed().as_secs_f64() * 1e6;
            if arm == "zero" {
                assert_eq!(result, baseline);
            }
            variants.push(
                json!({"setting":setting,"assembly_plus_propagation_us":micros,
                "hits":result.detailed.iter().map(|h|json!({"id":h.node_id,"score":h.score,
                    "rendered":h.included_in_context,"truncated":h.truncated_in_context,
                    "graph_only":!direct.contains(&h.node_id)})).collect::<Vec<_>>(),
                "context_tokens":assemble::default_token_count(&result.context),
                "warnings":result.warnings.len()}),
            );
        }
        rows.push(json!({"id":qid,"split":item["split"],"topic":item["topic"],"family":item["family"],
                         "phase1_count":phase1.len(),"graph_only_candidates":candidate_ids.iter().filter(|n|!direct.contains(n)).count(),
                         "variants":variants}));
    }
    assert_eq!(
        before,
        serde_json::to_vec(&graph.read().snapshot()).unwrap(),
        "experiment must not mutate graph"
    );
    assert_eq!(
        snapshot_sha256,
        fingerprint(&database),
        "snapshot bytes changed"
    );
    assert_eq!(
        dataset_sha256,
        fingerprint(&input),
        "labels changed during run"
    );
    if !cache_path.exists() {
        write_new(&cache_path, &vectors);
    }
    write_new(
        &output,
        &json!({"mode":mode,"embedding_contract":io.embedding,"clock":now,
        "snapshot_sha256":snapshot_sha256,"dataset_sha256":dataset_sha256,
        "method":"production phase1/expansion/daemon/hot-list/assembly; fixed frames; no GC or access updates",
        "queries":rows}),
    );
    println!(
        "#175 {mode}: {} queries; production-frame parity and immutable graph verified",
        rows.len()
    );
}
