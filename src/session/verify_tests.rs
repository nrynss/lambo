//! Synthetic alpha/beta/general coverage for the split verifier (#33 D3),
//! on the D2 fixture split built through the D1 and D2 tools. No rig data.

use super::*;
use crate::config::ServeConfig;
use crate::embed::{EmbedError, FixtureEmbedder};
use crate::session::split_tests::{id, plan_for, source};
use crate::session::{build_split, ClassificationPlan};

fn contract(dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim,
    }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// The serving table: every target pinned, `general-usage`-style default
/// (here alpha), `layers` as given.
fn serve(layers: &[&str]) -> ServeConfig {
    ServeConfig {
        sessions: strings(&["alpha", "beta", "general"]),
        default_session: Some("alpha".into()),
        layers: strings(layers),
        ..Default::default()
    }
}

fn queries() -> VerifyQueries {
    VerifyQueries {
        queries: BTreeMap::from([
            (
                "alpha".to_string(),
                strings(&[
                    "alpha cache is warm",
                    "beta parser is strict",
                    "operator-wide review conventions",
                    "Cargo.toml",
                    "README.md",
                ]),
            ),
            (
                "beta".to_string(),
                strings(&[
                    "beta result is green",
                    "alpha cache is warm",
                    "README.md",
                    "operator-wide review conventions",
                ]),
            ),
        ]),
    }
}

fn targets(split: &SplitOutput) -> BTreeMap<String, GraphSnapshot> {
    split
        .targets
        .iter()
        .map(|t| (t.session.clone(), t.snapshot.clone()))
        .collect()
}

fn mapped(split: &SplitOutput, n: u64, session: &str) -> NodeId {
    split
        .placements
        .iter()
        .find(|p| p.id == id(n))
        .unwrap()
        .targets[session]
}

/// An embedder whose every embed fails.
struct Failing;

#[async_trait::async_trait]
impl Embedder for Failing {
    fn dimensions(&self) -> usize {
        2
    }
    async fn embed(&self, _: &str) -> Result<Vec<f32>, EmbedError> {
        Err(EmbedError::Unavailable("down".into()))
    }
}

struct Run {
    source: GraphSnapshot,
    /// The source the split was built from (differs from `source` only in
    /// the digest-mismatch test).
    split_source: GraphSnapshot,
    targets: Option<BTreeMap<String, GraphSnapshot>>,
    serve: ServeConfig,
    queries: VerifyQueries,
    aggregate_only: BTreeSet<String>,
    dim: usize,
    top_k: usize,
    failing: bool,
    /// Added to the default clock (the source's newest row).
    later: chrono::Duration,
    /// Edits the rebuilt split before the run.
    edit: Option<fn(&mut SplitOutput)>,
}

impl Run {
    /// The D2 fixture, except that the general ruling's 2-d holder vector
    /// points away from every other concept's, so layered dedupe does not
    /// merge it into a project hit (see `layered_dedupe_is_not_a_loss`).
    fn new() -> Self {
        let mut source = source();
        let ruling = source.concepts.iter_mut().find(|c| c.id == id(17)).unwrap();
        ruling.embedding = Some(vec![-1.0, 0.0]);
        Self {
            split_source: source.clone(),
            source,
            targets: None,
            serve: serve(&["general"]),
            queries: queries(),
            aggregate_only: BTreeSet::new(),
            dim: 2,
            top_k: 10,
            failing: false,
            later: chrono::Duration::zero(),
            edit: None,
        }
    }

    fn plan(&self) -> ClassificationPlan {
        plan_for(&self.split_source)
    }

    fn split(&self) -> SplitOutput {
        build_split(&self.plan(), &self.split_source).unwrap()
    }

    async fn go(&self) -> Result<VerifyReport, VerifyError> {
        let plan = self.plan();
        let mut split = self.split();
        let targets = self.targets.clone().unwrap_or_else(|| targets(&split));
        if let Some(edit) = self.edit {
            edit(&mut split);
        }
        let fixture = FixtureEmbedder::with_dimensions(self.dim).unwrap();
        let embedder: &dyn Embedder = if self.failing { &Failing } else { &fixture };
        verify_split(VerifyInputs {
            source: self.source.clone(),
            plan: &plan,
            split: &split,
            targets,
            serve: &self.serve,
            queries: &self.queries,
            aggregate_only: &self.aggregate_only,
            params: VerifyParams {
                top_k: self.top_k,
                max_tokens: 4000,
                traversal_depth: 1,
                at: newest_row(&self.source).unwrap() + self.later,
            },
            embedder,
            embedding: &contract(self.dim),
            config: &Config::default(),
        })
        .await
    }
}

fn query<'a>(report: &'a VerifyReport, project: &str, text: &str) -> &'a QueryComparison {
    report.queries[project]
        .iter()
        .find(|q| q.query == text)
        .unwrap()
}

async fn refusal(run: &Run) -> String {
    run.go().await.unwrap_err().to_string()
}

#[tokio::test]
async fn the_report_is_deterministic_and_independent_of_source_row_order() {
    let run = Run::new();
    let first = serde_json::to_vec(&run.go().await.unwrap()).unwrap();
    let second = serde_json::to_vec(&run.go().await.unwrap()).unwrap();
    assert_eq!(first, second);

    let mut reordered = Run::new();
    reordered.source.interactions.reverse();
    reordered.source.concepts.reverse();
    reordered.source.edges.reverse();
    reordered.split_source = reordered.source.clone();
    let third = serde_json::to_vec(&reordered.go().await.unwrap()).unwrap();
    assert_eq!(first, third, "source row order changed the report");
}

/// Stale-session warnings are re-validated at read time; the pinned clock
/// keeps the report independent of the wall clock.
#[tokio::test]
async fn clock_bound_warnings_follow_the_pinned_clock_not_the_wall_clock() {
    let mut run = Run::new();
    run.later = chrono::Duration::days(10);
    let first = run.go().await.unwrap();
    let stale = first
        .queries
        .values()
        .flatten()
        .flat_map(|q| q.warnings_added.iter().chain(&q.warnings_removed))
        .any(|w| w.ends_with("stale_session"));
    assert!(stale, "ten days after the newest row a session is stale");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = run.go().await.unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
}

#[tokio::test]
async fn after_hits_map_back_to_their_origins_through_hub_copies() {
    let run = Run::new();
    let (plan, split) = (run.plan(), run.split());
    let report = run.go().await.unwrap();
    let origins = Origins::new(&split, &plan);
    for rows in report.queries.values() {
        for q in rows {
            for h in &q.before {
                assert_eq!(h.session, "mixed");
                assert_eq!(h.node_id, h.origin);
            }
            for h in &q.after {
                assert_ne!(h.node_id, h.origin, "a target id is never the source id");
                assert_eq!(origins.by_target[&(h.session.clone(), h.node_id)], h.origin);
                for (session, origin) in &h.also {
                    assert!(origins.placed[origin].1.contains(session));
                }
            }
        }
    }

    // Cargo.toml: assigned to beta, with a hub copy in alpha. Alpha's agent
    // finds alpha's copy, and it counts as the same memory as before.
    let cargo = query(&report, "alpha", "Cargo.toml");
    let hit = cargo.after.iter().find(|h| h.origin == id(20)).unwrap();
    assert_eq!(
        (hit.session.as_str(), hit.primary.as_str(), hit.hub),
        ("alpha", "beta", true)
    );
    assert_eq!(hit.node_id, mapped(&split, 20, "alpha"));
    assert!(!cargo.lost_ids.contains(&id(20)) && !cargo.gained_ids.contains(&id(20)));
    assert!(cargo.shifts.iter().any(|s| s.origin == id(20)));

    // README.md: assigned to alpha, with a hub copy in beta.
    let readme = query(&report, "beta", "README.md");
    let hit = readme.after.iter().find(|h| h.origin == id(13)).unwrap();
    assert_eq!(
        (hit.session.as_str(), hit.primary.as_str(), hit.hub),
        ("beta", "alpha", true)
    );
    assert_eq!(hit.node_id, mapped(&split, 13, "beta"));
    // `hub` is the plan's hub set, not "placed more than once": an
    // ordinary concept is never a hub.
    let plain = query(&report, "alpha", "alpha cache is warm");
    assert!(plain
        .after
        .iter()
        .filter(|h| h.origin == id(11))
        .all(|h| !h.hub));
}

#[test]
fn stub_replicas_and_primaries_map_back_to_one_origin() {
    let run = Run::new();
    let (plan, split) = (run.plan(), run.split());
    let origins = Origins::new(&split, &plan);
    // Interaction 3 is alpha's, replicated into beta as a stub (it is the
    // origin of beta concepts 15 and 16).
    let stub = mapped(&split, 3, "beta");
    let beta = &split.targets.iter().find(|t| t.session == "beta").unwrap();
    let row = beta.snapshot.interactions.iter().find(|i| i.id == stub);
    assert_eq!(row.unwrap().prompt_text, None);
    assert_eq!(origins.by_target[&("beta".to_string(), stub)], id(3));
    assert_eq!(
        origins.by_target[&("alpha".to_string(), mapped(&split, 3, "alpha"))],
        id(3)
    );
    let (primary, placed) = &origins.placed[&id(3)];
    assert_eq!(primary, "alpha");
    assert!(placed.contains("alpha") && placed.contains("beta"));
    assert!(!origins.hubs.contains(&id(3)));
    assert!(origins.hubs.contains(&id(13)) && origins.hubs.contains(&id(20)));
}

#[tokio::test]
async fn other_projects_hits_are_removed_and_foreign_primaries_are_counted() {
    let report = Run::new().go().await.unwrap();
    for summary in report.projects.values() {
        assert_eq!(summary.layers, vec!["general".to_string()]);
    }

    // Asked from alpha, beta's own ruling was a before hit and is gone after.
    let q = query(&report, "alpha", "beta parser is strict");
    assert!(q.before.iter().any(|h| h.origin == id(12)));
    assert!(q.removed_foreign_by_primary["beta"] >= 1);
    assert!(!q.after.iter().any(|h| h.origin == id(12)));

    // The real foreign-primary path: Cargo.toml is assigned to beta and
    // answers in alpha through its hub copy, so alpha's after side holds a
    // hit assigned to a session alpha does not read.
    let q = query(&report, "alpha", "Cargo.toml");
    assert_eq!(q.foreign_primary_after_by_primary["beta"], 1);
    let alpha = &report.projects["alpha"];
    assert!(alpha.foreign_primary_after >= 1);
    assert_eq!(
        alpha.foreign_primary_after,
        alpha
            .foreign_primary_after_by_primary
            .values()
            .sum::<usize>()
    );
    // Every after hit not assigned to alpha or general is counted.
    for q in &report.queries["alpha"] {
        let foreign = q
            .after
            .iter()
            .filter(|h| h.primary != "alpha" && h.primary != "general")
            .count();
        assert_eq!(
            foreign,
            q.foreign_primary_after_by_primary.values().sum::<usize>()
        );
    }

    // The operator ruling answers from the general layer, for both projects.
    for project in ["alpha", "beta"] {
        let q = query(&report, project, "operator-wide review conventions");
        let hit = q.after.iter().find(|h| h.origin == id(17)).unwrap();
        assert_eq!(hit.session, "general");
        assert!(!q.lost_ids.contains(&id(17)));
    }
    assert!(report.projects["alpha"].after_by_session["general"] >= 1);
    assert_eq!(
        report.layer_knobs["alpha"],
        vec![(
            "general".into(),
            layered::DEFAULT_LAYER_WEIGHT,
            layered::DEFAULT_LAYER_MIN_SIMILARITY
        )]
    );
    assert_eq!(report.pinned, strings(&["alpha", "beta", "general"]));
    assert_eq!(report.default_session, "alpha");
    assert!(report.measures.contains("SQLite holder"));
}

#[tokio::test]
async fn without_layers_general_is_out_of_scope() {
    let mut run = Run::new();
    run.serve = serve(&[]);
    let report = run.go().await.unwrap();
    let q = query(&report, "alpha", "operator-wide review conventions");
    assert!(q.removed_foreign_by_primary["general"] >= 1);
    assert!(q.after.iter().all(|h| h.session == "alpha"));
    assert!(report.projects["alpha"].layers.is_empty());
    assert!(report.layer_knobs.is_empty());
}

#[tokio::test]
async fn layered_dedupe_is_not_a_loss() {
    // Give every 2-d holder vector the same relevant query vector,
    // so the general ruling's copy passes admission and merges by layered
    // dedupe: the memory is still answered, as that hit's `also`.
    let mut run = Run::new();
    run.source = source();
    let vector = FixtureEmbedder::with_dimensions(2)
        .unwrap()
        .embed_sync("operator-wide review conventions");
    for c in &mut run.source.concepts {
        c.embedding = Some(vector.clone());
    }
    run.split_source = run.source.clone();
    let report = run.go().await.unwrap();
    let q = query(&report, "alpha", "operator-wide review conventions");
    assert!(q.deduped_ids.contains(&id(17)), "{q:#?}");
    assert!(!q.lost_ids.contains(&id(17)));
    assert!(q
        .after
        .iter()
        .any(|h| h.also.contains(&("general".to_string(), id(17)))));
    assert!(report.projects["alpha"].deduped >= 1);
}

/// Disclosure: an aggregate-only session (and any session nobody queried)
/// appears only as counts, and no raw warning line is kept.
#[tokio::test]
async fn aggregate_only_sessions_appear_only_as_counts() {
    let mut run = Run::new();
    run.queries.queries.remove("beta");
    run.aggregate_only = BTreeSet::from(["beta".to_string()]);
    run.later = chrono::Duration::days(10);
    let split = run.split();
    let report = run.go().await.unwrap();
    let json = serde_json::to_string(&report).unwrap();
    for p in split.placements.iter().filter(|p| p.primary == "beta") {
        assert!(
            !json.contains(&p.id.0.to_string()),
            "a beta origin id leaked"
        );
        for t in p.targets.values() {
            assert!(!json.contains(&t.0.to_string()), "a beta target id leaked");
        }
    }
    let q = query(&report, "alpha", "beta parser is strict");
    assert!(q.before_undisclosed_by_primary["beta"] >= 1);
    assert!(q.before.iter().all(|h| h.primary != "beta"));
    let cargo = query(&report, "alpha", "Cargo.toml");
    assert_eq!(cargo.after_undisclosed_by_primary["beta"], 1);
    assert_eq!(cargo.foreign_primary_after_by_primary["beta"], 1);
    // Counts still cover every hit.
    let alpha = &report.projects["alpha"];
    let shown: usize = report.queries["alpha"]
        .iter()
        .map(|q| q.after.len() + q.after_undisclosed_by_primary.values().sum::<usize>())
        .sum();
    assert_eq!(shown, alpha.after_hits);
    // Warning kinds only: no raw line ("Session inactive N seconds").
    assert!(!json.contains("Session inactive"));
    assert!(alpha
        .warnings_added
        .keys()
        .any(|k| k.ends_with("stale_session")));
    assert_eq!(report.aggregate_only, strings(&["beta"]));

    // An aggregate-only session cannot be queried or read as a layer.
    let mut run = Run::new();
    run.aggregate_only = BTreeSet::from(["beta".to_string()]);
    assert!(refusal(&run).await.contains("beta is aggregate-only"));
    let mut run = Run::new();
    run.queries.queries.remove("beta");
    run.aggregate_only = BTreeSet::from(["general".to_string()]);
    assert!(refusal(&run).await.contains("reads aggregate-only general"));
    let mut run = Run::new();
    run.aggregate_only = BTreeSet::from(["gamma".to_string()]);
    assert!(refusal(&run).await.contains("gamma is not a target"));
}

#[test]
fn warning_kinds_keep_no_text() {
    let cases = [
        ("Session inactive 3600 seconds", "stale_session"),
        (
            "[general] Session inactive 1 seconds",
            "[general] stale_session",
        ),
        ("Agent B wrote to it 12 seconds ago", "conflict"),
        ("High-risk modification: secret reason", "high_risk"),
        (
            "⚠ Contradicted at 2026-01-01T00:00:00Z by x: \"text\" (id 1)",
            "contradicted",
        ),
        ("⚠ Contradicts \"text\" (id 1)", "contradicts"),
        (
            "Reserved by agent-c until 2025-07-08T18:45:00Z",
            "reservation",
        ),
        (
            "recall: layer general is unavailable; results are from a",
            "layer_unavailable",
        ),
        ("something else entirely", "other"),
    ];
    for (line, kind) in cases {
        assert_eq!(warning_kind(line), kind, "{line}");
    }
}

/// A recall@k cut at rank `k` on both sides: forced hits past `k` do not
/// count.
#[tokio::test]
async fn recall_at_k_counts_ranks_up_to_k_only() {
    let mut run = Run::new();
    run.top_k = 2;
    let report = run.go().await.unwrap();
    for q in report.queries.values().flatten() {
        assert!(q.before_in_scope_at_k <= 2);
        assert!(q.overlap_at_k <= q.before_in_scope_at_k);
        assert!(q.before_in_scope_at_k <= q.before_in_scope);
    }
}

#[tokio::test]
async fn the_serving_table_decides_pinning_and_layers() {
    // A queried project the serve does not pin is attached on demand,
    // unlayered: refused.
    let mut run = Run::new();
    run.serve.sessions = strings(&["alpha", "general"]);
    assert!(refusal(&run).await.contains("beta is not pinned"));

    // A layer that is not pinned: the serve's own start check refuses.
    let mut run = Run::new();
    run.serve.sessions = strings(&["alpha", "beta"]);
    assert!(refusal(&run).await.contains("serving [serve] table"));

    // A default that is not pinned, likewise.
    let mut run = Run::new();
    run.serve.default_session = Some("elsewhere".into());
    assert!(refusal(&run).await.contains("default_session"));

    // Zero rows: a pinned session the split wrote nothing into cannot be
    // queried or layered.
    let mut run = Run::new();
    run.serve.sessions.push("gamma".into());
    run.queries
        .queries
        .insert("gamma".into(), strings(&["anything"]));
    assert!(refusal(&run).await.contains("wrote no rows"));
    let mut run = Run::new();
    run.serve.sessions.push("gamma".into());
    run.serve.layers = strings(&["gamma"]);
    run.queries.queries.remove("beta");
    assert!(refusal(&run).await.contains("layered with gamma"));
}

#[tokio::test]
async fn mismatched_outputs_contracts_and_query_sets_are_refused() {
    let mut run = Run::new();
    let mut edited = targets(&run.split());
    edited
        .get_mut("alpha")
        .unwrap()
        .concepts
        .first_mut()
        .unwrap()
        .content
        .push_str(" (edited)");
    run.targets = Some(edited);
    assert!(refusal(&run).await.contains("target alpha does not match"));

    let mut run = Run::new();
    let mut fewer = targets(&run.split());
    fewer.remove("beta");
    run.targets = Some(fewer);
    assert!(refusal(&run).await.contains("not the split's targets"));

    let mut run = Run::new();
    run.queries
        .queries
        .insert("gamma".into(), vec!["anything".into()]);
    assert!(refusal(&run).await.contains("gamma, which is not a target"));

    let mut run = Run::new();
    run.queries.queries.insert("alpha".into(), vec![]);
    assert!(refusal(&run).await.contains("1 to"));

    // An embedder whose contract is not the store's.
    let mut run = Run::new();
    run.dim = 3;
    assert!(refusal(&run).await.contains("session mixed"));

    let mut run = Run::new();
    run.top_k = 0;
    assert!(refusal(&run).await.contains("top_k"));

    // A query that does not embed refuses the whole run.
    let mut run = Run::new();
    run.failing = true;
    assert!(refusal(&run).await.contains("did not embed"));

    // A source that is not the split's: the digests disagree.
    let mut run = Run::new();
    run.source.concepts[0].content.push_str(" (other)");
    assert!(refusal(&run).await.contains("digests disagree"));

    // Placements that do not map the output's ids back: refused, never
    // reported as a hit.
    let mut run = Run::new();
    run.edit = Some(|split| {
        for p in &mut split.placements {
            p.targets.retain(|s, _| s != "alpha");
        }
    });
    assert!(refusal(&run).await.contains("has no placement"));
}

/// M3 parity: on a real D2 SQLite split output, the replay's layered recall
/// equals `Memory::recall_layered` from attached SQLite holders (alpha with
/// the general layer), hit for hit: session, id, rank and dedupe copies.
#[cfg(feature = "store-sqlite")]
#[tokio::test(flavor = "multi_thread")]
async fn the_replay_matches_memory_recall_layered_on_a_sqlite_split_output() {
    use crate::memory::{LayerRef, Memory};
    use crate::resolve::SplitDestination;
    use crate::store::{GraphStore, SplitReceipt, SqliteStore};
    use std::time::Duration;

    // The D2 output, written by the split's own import.
    let run = Run::new();
    let split = run.split();
    let dir = std::env::temp_dir().join(format!("lambo-verify-parity-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("split.db");
    std::fs::File::create(&path).unwrap();
    let names: BTreeSet<String> = split.targets.iter().map(|t| t.session.clone()).collect();
    let destination = SplitDestination::open(&path);
    destination.provision_split_destination().await.unwrap();
    for t in &split.targets {
        let receipt = SplitReceipt::for_target(
            &t.snapshot,
            &split.split_sha256,
            &t.sha256,
            split.algorithm_version,
            &split.source_sha256,
            &split.plan_sha256,
        );
        destination
            .import_split_target(&receipt, &t.snapshot, &names)
            .await
            .unwrap();
    }
    destination.finish_split_destination().await.unwrap();

    // Holders attach the output as a serve would (SQLite: holder-graph
    // vectors), with a long tick so reads never move the scores.
    let store: Arc<dyn GraphStore> = Arc::new(
        SqliteStore::connect(path.to_str().unwrap())
            .unwrap()
            .with_vector_dim(2)
            .unwrap(),
    );
    let config = Config {
        daemon_tick_interval: Duration::from_secs(3_600),
        ..Config::default()
    };
    let holder = |session: &'static str| {
        Memory::builder()
            .session(session)
            .agent("verify-parity")
            .config(config.clone())
            .flush_interval(Duration::from_secs(3_600))
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::with_dimensions(2).unwrap()) as Arc<dyn Embedder>)
            .embedding_contract(contract(2))
            .build()
    };
    let alpha = holder("alpha").await.unwrap();
    let general = holder("general").await.unwrap();
    alpha.settle_daemon().await;
    general.settle_daemon().await;

    // The replay, over the same rows, at the holders' clock (wall clock;
    // the fixture's data is months old, so every window agrees).
    let at = Utc::now();
    let ctx = Ctx {
        embedding: &contract(2),
        config: &config,
    };
    let rows = targets(&split);
    let replay_alpha = Replay::new(rows["alpha"].clone(), &config, ctx.embedding, at).unwrap();
    let replay_general = Replay::new(rows["general"].clone(), &config, ctx.embedding, at).unwrap();
    let spec = LayerSpec {
        session: "general".into(),
        weight: layered::DEFAULT_LAYER_WEIGHT,
        min_similarity: layered::DEFAULT_LAYER_MIN_SIMILARITY,
    };
    let params = VerifyParams {
        top_k: 10,
        max_tokens: 4000,
        traversal_depth: 1,
        at,
    };
    let embedder = FixtureEmbedder::with_dimensions(2).unwrap();
    type Key = Vec<(String, NodeId, usize, Vec<(String, NodeId)>)>;
    let key = |r: &LayeredRecall| -> Key {
        r.hits
            .iter()
            .enumerate()
            .map(|(i, h)| {
                (
                    h.session.0.clone(),
                    h.hit.node_id,
                    i + 1,
                    h.also
                        .iter()
                        .map(|c| (c.session.0.clone(), c.node_id))
                        .collect(),
                )
            })
            .collect()
    };
    let mut compared = 0;
    for text in &queries().queries["alpha"] {
        let held = alpha
            .recall_layered(&[LayerRef::new(&general)], recall_query(text, &params))
            .await
            .unwrap();
        let vector = embedder.embed_query(text).await.unwrap();
        let replayed = layered_recall(
            &replay_alpha,
            &[(&replay_general, &spec)],
            text,
            &vector,
            &params,
            &ctx,
        )
        .await;
        assert_eq!(key(&replayed), key(&held), "query {text:?}");
        compared += held.hits.len();
    }
    assert!(compared > 0, "the parity check compared no hits");
    alpha.close().await.unwrap();
    general.close().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
