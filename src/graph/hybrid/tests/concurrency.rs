//! #116 and #102: concurrent commits, plan validation, the embed cache and
//! the in-place commit's rollback.
//!
//! The embedders here map a content to a vector the test chooses, so the
//! merge geometry is exact: a plan's probe, its merge target and a rival's
//! vector sit at known cosines. They read the content back out of the
//! framed context ([`context_text`]).

use super::*;
use crate::graph::vector_source::GraphVectorSource;
use crate::store::vector_source::VectorCandidates;

const DIM: usize = 4;

/// The content a framed embed text was built from.
fn content_of(text: &str) -> &str {
    match text.split_once(" — ") {
        Some((content, _)) => content,
        None => text.strip_prefix("Concept: ").unwrap_or(text),
    }
}

/// A unit-ish vector at cosine `c` from `[1, 0, 0, 0]`, off along `axis`.
fn at_cosine(c: f32, axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; DIM];
    v[0] = c;
    v[axis] = (1.0 - c * c).sqrt();
    v
}

fn probe() -> Vec<f32> {
    vec![1.0, 0.0, 0.0, 0.0]
}

/// Maps contents to vectors; may pause on one content until released, or
/// hold its first `barrier` calls at a barrier. Counts every embed.
struct TableEmbedder {
    table: Vec<(&'static str, Vec<f32>)>,
    pause_on: Option<&'static str>,
    started: Notify,
    release: Notify,
    barrier: Option<Barrier>,
    calls: AtomicUsize,
}

impl TableEmbedder {
    fn new(table: Vec<(&'static str, Vec<f32>)>) -> Self {
        Self {
            table,
            pause_on: None,
            started: Notify::new(),
            release: Notify::new(),
            barrier: None,
            calls: AtomicUsize::new(0),
        }
    }
    fn pausing_on(mut self, content: &'static str) -> Self {
        self.pause_on = Some(content);
        self
    }
    fn with_barrier(mut self, parties: usize) -> Self {
        self.barrier = Some(Barrier::new(parties));
        self
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Embedder for TableEmbedder {
    fn dimensions(&self) -> usize {
        DIM
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let content = content_of(text);
        if let Some(barrier) = &self.barrier
            && call < 2
        {
            barrier.wait().await;
        }
        if self.pause_on == Some(content) {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.table
            .iter()
            .find(|(c, _)| *c == content)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| EmbedError::Unavailable(format!("no table entry for {content:?}")))
    }
}

fn no_synonym(_: &str) -> Option<&str> {
    None
}

fn table_contract() -> EmbeddingContract {
    contract("table", DIM)
}

/// A stamped session holding one interaction.
fn stamped_session(sess: &str) -> (Arc<RwLock<Graph>>, NodeId) {
    let (graph, interaction) = graph_with_interaction(sess, 1, 0, "concurrent writers");
    graph.write().stamp_embedding(table_contract()).unwrap();
    (graph, interaction)
}

/// Insert a concept carrying `vector` straight into the graph, as a rival
/// writer's commit would leave it.
fn seed(graph: &RwLock<Graph>, interaction: NodeId, content: &str, vector: Vec<f32>) -> NodeId {
    let mut g = graph.write();
    let key = crate::graph::canonical::canonical_key(content, no_synonym);
    let session = g.session_id().clone();
    let c = new_concept(
        &session,
        content,
        ConceptType::Entity,
        key,
        interaction,
        &AgentId::from("rival"),
        ts(0),
        Some(vector),
        None,
    );
    let id = c.id;
    g.insert_concept(c, interaction).unwrap();
    id
}

/// One hybrid derive over the holder's graph (the exact in-RAM source).
async fn derive_over_graph(
    graph: Arc<RwLock<Graph>>,
    embedder: Arc<TableEmbedder>,
    interaction: NodeId,
    concepts: Vec<(&'static str, ConceptType)>,
    pairs: Vec<(&'static str, &'static str)>,
) -> Result<DeriveOutcome, LamboError> {
    let parent_of = if pairs.is_empty() {
        ParentOf::none()
    } else {
        ParentOf::from_pairs(&pairs)
    };
    derive_with(
        graph.clone(),
        VectorCandidates::Graph(GraphVectorSource::new(&graph)),
        embedder.as_ref(),
        &table_contract(),
        interaction,
        &agent(),
        &concepts,
        &parent_of,
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
        None,
    )
    .await
}

/// #116's acceptance case: two derives on unrelated keys, both planned
/// before either commits (the barrier holds both inside their embed). The
/// second commit finds the epoch moved, checks its plan against the first
/// one's concept, and commits: one attempt each, one embed each.
#[tokio::test]
async fn two_derives_racing_on_unrelated_keys_both_commit_without_a_replan() {
    let (graph, interaction) = stamped_session("hybrid-race-unrelated");
    let embedder = Arc::new(
        TableEmbedder::new(vec![
            ("alpha topic", vec![1.0, 0.0, 0.0, 0.0]),
            ("beta topic", vec![0.0, 1.0, 0.0, 0.0]),
        ])
        .with_barrier(2),
    );
    let attempts = plan_attempts();
    let spawn = |content: &'static str| {
        let graph = graph.clone();
        let embedder = embedder.clone();
        tokio::spawn(async move {
            derive_over_graph(
                graph,
                embedder,
                interaction,
                vec![(content, ConceptType::Entity)],
                Vec::new(),
            )
            .await
        })
    };
    let (a, b) = tokio::join!(spawn("alpha topic"), spawn("beta topic"));
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());

    assert_eq!((a.created.len(), b.created.len()), (1, 1));
    assert_eq!(plan_attempts() - attempts, 2, "no replan on either side");
    assert_eq!(embedder.calls(), 2, "one embed each, none repeated");
    let g = graph.read();
    assert_eq!(g.concepts().count(), 2);
    g.assert_invariants().unwrap();
}

/// A rival commits a concept with this call's canonical key (another
/// spelling of it) between plan and commit. The plan is stale, the replan
/// matches the rival's concept, and no second concept with that key is
/// written. The replan embeds nothing: a canonical match needs no vector.
#[tokio::test]
async fn a_rival_commit_of_the_same_canonical_key_is_matched_not_duplicated() {
    let (graph, interaction) = stamped_session("hybrid-race-same-key");
    let embedder =
        Arc::new(TableEmbedder::new(vec![("shared topic", probe())]).pausing_on("shared topic"));
    let attempts = plan_attempts();
    let task = tokio::spawn(derive_over_graph(
        graph.clone(),
        embedder.clone(),
        interaction,
        vec![("shared topic", ConceptType::Entity)],
        Vec::new(),
    ));
    embedder.started.notified().await;
    let rival = seed(&graph, interaction, "Shared Topics", at_cosine(0.1, 1));
    embedder.release.notify_one();
    let out = task.await.unwrap().unwrap();

    assert_eq!(out.matched, vec![rival], "{out:?}");
    assert!(out.created.is_empty(), "{out:?}");
    assert_eq!(
        plan_attempts() - attempts,
        2,
        "the stale plan was replanned"
    );
    assert_eq!(embedder.calls(), 1);
    let g = graph.read();
    let key = crate::graph::canonical::canonical_key("shared topic", no_synonym);
    assert_eq!(
        g.concepts().filter(|c| c.canonical_key == key).count(),
        1,
        "one concept per canonical key"
    );
    g.assert_invariants().unwrap();
}

/// The merge scenarios pause the call on its `parent_of` end, which is
/// embedded after the item's candidates were ranked, so a rival written in
/// the pause lands between the merge decision and the commit.
async fn merge_race(
    sess: &str,
    rival: impl FnOnce(&RwLock<Graph>, NodeId, NodeId) -> Option<NodeId>,
) -> (DeriveOutcome, NodeId, Option<NodeId>, usize, usize) {
    let (graph, interaction) = stamped_session(sess);
    let target = seed(&graph, interaction, "planned target", at_cosine(0.90, 1));
    let embedder = Arc::new(
        TableEmbedder::new(vec![
            ("the probe", probe()),
            ("probe parent", vec![0.0, 0.0, 0.0, 1.0]),
            ("probe child", vec![0.0, 0.0, 1.0, 0.0]),
        ])
        .pausing_on("probe parent"),
    );
    let attempts = plan_attempts();
    let task = tokio::spawn(derive_over_graph(
        graph.clone(),
        embedder.clone(),
        interaction,
        vec![("the probe", ConceptType::Entity)],
        vec![("probe parent", "probe child")],
    ));
    embedder.started.notified().await;
    let rival_id = rival(&graph, interaction, target);
    embedder.release.notify_one();
    let out = task.await.unwrap().unwrap();
    graph.read().assert_invariants().unwrap();
    (
        out,
        target,
        rival_id,
        plan_attempts() - attempts,
        embedder.calls(),
    )
}

/// Design decision (#116): a rival committed after the plan that would win
/// the semantic merge (it scores above the planned target) makes the plan
/// stale. The replan merges into the rival, exactly as a call that started
/// after it would have, and embeds nothing again.
#[tokio::test]
async fn a_rival_that_would_win_the_semantic_merge_forces_a_replan() {
    let (out, _target, rival, attempts, embeds) =
        merge_race("hybrid-race-better-rival", |g, i, _| {
            Some(seed(g, i, "stronger rival", at_cosine(0.95, 2)))
        })
        .await;
    assert_eq!(out.semantic_merged, vec![rival.unwrap()], "{out:?}");
    assert_eq!(attempts, 2);
    assert_eq!(embeds, 3, "the probe and both ends, each once");
}

/// The control: a rival above the threshold but below the planned target
/// cannot change the choice, so the plan holds and commits as planned.
#[tokio::test]
async fn a_weaker_rival_does_not_disturb_the_planned_merge() {
    let (out, target, _rival, attempts, embeds) =
        merge_race("hybrid-race-weaker-rival", |g, i, _| {
            Some(seed(g, i, "weaker rival", at_cosine(0.86, 3)))
        })
        .await;
    assert_eq!(out.semantic_merged, vec![target], "{out:?}");
    assert_eq!(attempts, 1, "no replan");
    assert_eq!(embeds, 3);
}

/// A change to a planned merge target (here its removal) makes the plan
/// stale. Without the check the in-place commit would find the target gone
/// and write the concept keyword-only; the replan finds no candidate above
/// the threshold and writes a fresh concept that keeps its vector.
#[tokio::test]
async fn a_removed_merge_target_forces_a_replan() {
    let (out, _target, _rival, attempts, embeds) =
        merge_race("hybrid-race-removed-target", |g, _, target| {
            g.write().remove_node(target).unwrap();
            None
        })
        .await;
    assert!(out.semantic_merged.is_empty(), "{out:?}");
    assert_eq!(out.created.len(), 3, "the probe and both ends: {out:?}");
    assert_eq!(out.embedded, 3, "the probe kept its vector: {out:?}");
    assert_eq!(attempts, 2);
    assert_eq!(embeds, 3);
}

/// #102: a commit that fails after it has already written (here the second
/// item's vector has the wrong width, which only `insert_concept` checks)
/// leaves the graph exactly as it was: no first concept, no contract stamp,
/// no log entries, no epoch movement, nothing marked unflushed.
#[tokio::test]
async fn a_commit_that_fails_midway_leaves_the_graph_untouched() {
    let (graph, interaction) = graph_with_interaction("hybrid-midway", 1, 0, "midway failure");
    let embedder = Arc::new(TableEmbedder::new(vec![
        ("good item", probe()),
        ("bad item", vec![1.0, 0.0, 0.0]),
    ]));
    let store = SpyStore::with_vector(Vec::new());
    let before = {
        let g = graph.read();
        (
            g.snapshot(),
            g.epoch(),
            g.log_len(),
            g.unflushed_len(),
            g.contract_unflushed(),
        )
    };
    let err = derive(
        graph.clone(),
        &store,
        embedder.as_ref(),
        &table_contract(),
        interaction,
        &agent(),
        &[
            ("good item", ConceptType::Entity),
            ("bad item", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("width"), "{err}");
    let g = graph.read();
    let after = (
        g.snapshot(),
        g.epoch(),
        g.log_len(),
        g.unflushed_len(),
        g.contract_unflushed(),
    );
    assert_eq!(after, before);
    assert!(g.embedding().is_none(), "the stamp was undone too");
    g.assert_invariants().unwrap();
}

/// The J3 commit hook still runs exactly once, after a replan, under the
/// commit's lock hold, and sees the mutations it travels with.
#[tokio::test]
async fn the_commit_hook_runs_once_after_a_replan() {
    let (graph, interaction) = stamped_session("hybrid-hook-replan");
    let embedder =
        Arc::new(TableEmbedder::new(vec![("hooked topic", probe())]).pausing_on("hooked topic"));
    let runs = Arc::new(AtomicUsize::new(0));
    let hook: CommitHook = {
        let runs = runs.clone();
        Box::new(move |g: &mut Graph, outcome: &DeriveOutcome| {
            runs.fetch_add(1, Ordering::SeqCst);
            assert!(outcome.matched.iter().all(|id| g.node(*id).is_some()));
        })
    };
    let task = {
        let graph = graph.clone();
        let embedder = embedder.clone();
        tokio::spawn(async move {
            derive_with(
                graph.clone(),
                VectorCandidates::Graph(GraphVectorSource::new(&graph)),
                embedder.as_ref(),
                &table_contract(),
                interaction,
                &agent(),
                &[("hooked topic", ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                None,
                Some(hook),
            )
            .await
        })
    };
    embedder.started.notified().await;
    seed(&graph, interaction, "hooked topics", at_cosine(0.1, 1));
    embedder.release.notify_one();
    let out = task.await.unwrap().unwrap();
    assert_eq!(out.matched.len(), 1, "{out:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}
