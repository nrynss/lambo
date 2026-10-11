//! #102: what one hybrid commit costs under the write lock, by session size.
//!
//! Ignored: it builds sessions of 10k, 50k and 100k concepts with 1,024-wide
//! vectors and is meant for a release build, by hand:
//!
//! ```text
//! cargo test --release --lib graph::hybrid::tests::commit_cost -- --ignored --nocapture
//! ```
//!
//! It times only the commit phase (write lock taken to commit returned), the
//! section #102 is about, through the test-only `last_commit_duration` probe.

use super::*;

const DIM: usize = 1024;

/// A deterministic, non-zero, pairwise-distinct vector for concept `i`.
fn vector(i: u64) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[(i as usize) % DIM] = 1.0;
    v[((i / DIM as u64) as usize + 7) % DIM] += 0.5;
    v
}

/// A session of `n` embedded concepts, loaded the way a writer loads one.
fn session(n: u64) -> (Arc<RwLock<Graph>>, NodeId) {
    let sess = sid("hybrid-commit-cost");
    let mut i = interaction(1, None, 0, "commit cost probe");
    i.session_id = sess.clone();
    let iid = i.id;
    let mut concepts = Vec::with_capacity(n as usize);
    let mut edges = Vec::with_capacity(n as usize);
    for k in 0..n {
        let id = NodeId(Uuid::from_u64_pair(7, k));
        concepts.push(Concept {
            id,
            session_id: sess.clone(),
            content: format!("bulk concept {k}"),
            canonical_key: format!("bulk concept {k}"),
            concept_type: ConceptType::Entity,
            origin_interaction: iid,
            origin_agent: agent(),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: Some(vector(k)),
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        });
        edges.push(Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(8, k)),
            session_id: sess.clone(),
            source: iid,
            target: id,
            edge_type: EdgeType::Derives,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        });
    }
    let snap = crate::types::GraphSnapshot {
        session_id: sess,
        interactions: vec![i],
        concepts,
        edges,
        embedding: Some(contract("cost", DIM)),
        ..Default::default()
    };
    (
        Arc::new(RwLock::new(Graph::from_snapshot(snap).unwrap())),
        iid,
    )
}

/// An embedder that returns one fixed vector, so the gather phase is free.
#[derive(Debug)]
struct ConstEmbedder;

#[async_trait]
impl Embedder for ConstEmbedder {
    fn dimensions(&self) -> usize {
        DIM
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = vec![0.0f32; DIM];
        v[3] = 1.0;
        v[11] = 1.0;
        Ok(v)
    }
}

/// Median commit time of `rounds` two-concept derives on a session of `n`.
async fn median_commit(n: u64, rounds: usize) -> Duration {
    let (graph, interaction) = session(n);
    let store = SpyStore::with_vector(Vec::new());
    let mut samples = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let a = format!("probe alpha {r}");
        let b = format!("probe beta {r}");
        derive(
            graph.clone(),
            &store,
            &ConstEmbedder,
            &contract("cost", DIM),
            interaction,
            &agent(),
            &[
                (a.as_str(), ConceptType::Entity),
                (b.as_str(), ConceptType::Entity),
            ],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            None,
        )
        .await
        .unwrap();
        samples.push(last_commit_duration().expect("the derive committed"));
    }
    samples.sort();
    samples[rounds / 2]
}

#[tokio::test]
#[ignore = "benchmark: run by hand in release (#102)"]
async fn commit_cost_does_not_scale_with_session_size() {
    let mut rows = Vec::new();
    for n in [10_000u64, 50_000, 100_000] {
        let median = median_commit(n, 21).await;
        println!("hybrid commit, {n} concepts x {DIM} dims: median {median:?}");
        rows.push((n, median));
    }
    // Generous: the clone this replaces cost 6.8 ms at 10k and 36 ms at 50k
    // in this probe (release, M3 Pro; #102 measured 10.7 / 60 / 168 ms at
    // 10k / 50k / 100k). A commit that still scaled with the session would
    // show the 10x size step; allow scheduler noise, not a trend.
    let (_, small) = rows[0];
    let (_, large) = rows[2];
    assert!(
        large < small * 3 + Duration::from_millis(1),
        "commit cost grew with the session: {small:?} at 10k, {large:?} at 100k"
    );
}

/// Review L1: the worst plan validation the cap admits, timed. A plan with
/// 32 ranked items (fresh concepts) checked against `MAX_PLAN_RESCORES / 32`
/// concepts written since it was made, each with a 1,024-wide vector: exactly
/// `MAX_PLAN_RESCORES` cosines, none near an item, so the plan holds and
/// every cosine is computed.
#[test]
#[ignore = "benchmark: run by hand in release (#116 review L1)"]
fn plan_validation_worst_case_is_bounded() {
    const ITEMS: usize = 32;
    const WRITTEN: usize = MAX_PLAN_RESCORES / ITEMS;
    let (graph, interaction) = session(10_000);
    let contents: Vec<String> = (0..ITEMS).map(|k| format!("validation item {k}")).collect();
    let concepts: Vec<(&str, ConceptType)> = contents
        .iter()
        .map(|c| (c.as_str(), ConceptType::Entity))
        .collect();
    let mut g = graph.write();
    let planned_epoch = g.epoch();
    let (items, parent_ends) = plan_inputs(&g, &concepts, &ParentOf::none()).unwrap();
    let mut probe = vec![0.0f32; DIM];
    probe[3] = 1.0;
    probe[11] = 1.0;
    let resolutions: Vec<Resolution> = items
        .iter()
        .map(|(_, _, key, _)| Resolution::Fresh {
            key: key.clone(),
            embedding: Some(probe.clone()),
            source: None,
        })
        .collect();
    let session_id = g.session_id().clone();
    for k in 0..WRITTEN as u64 {
        let c = new_concept(
            &session_id,
            &format!("written since the plan {k}"),
            ConceptType::Entity,
            format!("written since the plan {k}"),
            interaction,
            &agent(),
            ts(0),
            Some(vector(100_000 + k)),
            None,
        );
        g.insert_concept(c, interaction).unwrap();
    }
    let stamped = g.embedding().cloned();
    let (created_at, event_time, prompt) = match g.node(interaction) {
        Some(Node::Interaction(i)) => (i.created_at, i.event_time, i.prompt_text.clone()),
        _ => unreachable!(),
    };
    let plan = PlanRead {
        planned_epoch,
        interaction,
        interaction_read: (prompt.as_deref(), created_at, event_time),
        stamped: stamped.as_ref(),
        concepts: &concepts,
        parent_of: &ParentOf::none(),
        items: &items,
        parent_ends: &parent_ends,
        resolutions: &resolutions,
        semantic_match_threshold: SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        rescore_cap: MAX_PLAN_RESCORES,
    };
    let mut samples = Vec::new();
    for _ in 0..21 {
        let started = std::time::Instant::now();
        assert_eq!(plan_still_holds(&g, &plan).unwrap(), Verdict::Holds);
        samples.push(started.elapsed());
    }
    samples.sort();
    println!(
        "plan validation, {ITEMS} items x {WRITTEN} written concepts x {DIM} dims \
         ({MAX_PLAN_RESCORES} cosines): median {:?}",
        samples[samples.len() / 2]
    );
}
