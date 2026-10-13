//! Synthetic alpha/beta/general coverage for the split verifier (#33 D3),
//! on the D2 fixture split built through the D1 and D2 tools. No rig data.

use super::*;
use crate::config::ServeConfig;
use crate::embed::FixtureEmbedder;
use crate::recall::detail::DetailedRecall;
use crate::session::build_split;
use crate::session::split_tests::{id, plan_for, source};

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 2,
    }
}

fn layers(split: &SplitOutput, stack: &[&str]) -> ServeLayers {
    let pinned: Vec<String> = split.targets.iter().map(|t| t.session.clone()).collect();
    ServeConfig {
        layers: stack.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    }
    .resolve_layers(&pinned)
    .unwrap()
}

fn queries() -> VerifyQueries {
    let q = |list: &[&str]| list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    VerifyQueries {
        queries: BTreeMap::from([
            (
                "alpha".to_string(),
                q(&[
                    "alpha cache is warm",
                    "beta parser is strict",
                    "operator-wide review conventions",
                    "Cargo.toml",
                    "README.md",
                ]),
            ),
            (
                "beta".to_string(),
                q(&["beta result is green", "alpha cache is warm", "README.md"]),
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

struct Run {
    source: GraphSnapshot,
    targets: BTreeMap<String, GraphSnapshot>,
    stack: Vec<&'static str>,
    queries: VerifyQueries,
    dim: usize,
    top_k: usize,
    /// Added to the default clock (the source's newest row).
    later: chrono::Duration,
}

impl Run {
    /// The D2 fixture, except that the general ruling's 2-d holder vector
    /// points away from every other concept's, so layered dedupe does not
    /// merge it into a project hit (see `layered_dedupe_is_not_a_loss`).
    fn new() -> Self {
        let mut source = source();
        let ruling = source.concepts.iter_mut().find(|c| c.id == id(17)).unwrap();
        ruling.embedding = Some(vec![-1.0, 0.0]);
        let split = build_split(&plan_for(&source), &source).unwrap();
        Self {
            targets: targets(&split),
            source,
            stack: vec!["general"],
            queries: queries(),
            dim: 2,
            top_k: 10,
            later: chrono::Duration::zero(),
        }
    }

    async fn go(&self) -> Result<VerifyReport, VerifyError> {
        let split = build_split(&plan_for(&self.source), &self.source).unwrap();
        let layers = layers(&split, &self.stack);
        let embedder = FixtureEmbedder::with_dimensions(self.dim).unwrap();
        let embedding = EmbeddingContract {
            dim: self.dim,
            ..contract()
        };
        verify_split(VerifyInputs {
            source: self.source.clone(),
            split: &split,
            targets: self.targets.clone(),
            layers: &layers,
            queries: &self.queries,
            params: VerifyParams {
                top_k: self.top_k,
                max_tokens: 4000,
                traversal_depth: 1,
                at: newest_row(&self.source).unwrap() + self.later,
            },
            embedder: &embedder,
            embedding: &embedding,
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
    let third = serde_json::to_vec(&reordered.go().await.unwrap()).unwrap();
    assert_eq!(first, third, "source row order changed the report");
}

/// Stale-session warnings are re-validated at read time; the pinned clock
/// keeps their seconds, and so the report, independent of the wall clock.
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
        .any(|w| w.contains("Session inactive"));
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
    let split = build_split(&plan_for(&run.source), &run.source).unwrap();
    let report = run.go().await.unwrap();

    // Every before hit is a source row; every after hit maps through the
    // placements to the origin it was copied from.
    let origins = Origins::new(&split);
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
    assert!(!cargo.lost.contains(&id(20)) && !cargo.gained.contains(&id(20)));
    assert!(cargo.shifts.iter().any(|s| s.origin == id(20)));

    // README.md: assigned to alpha, with a hub copy in beta.
    let readme = query(&report, "beta", "README.md");
    let hit = readme.after.iter().find(|h| h.origin == id(13)).unwrap();
    assert_eq!(
        (hit.session.as_str(), hit.primary.as_str(), hit.hub),
        ("beta", "alpha", true)
    );
    assert_eq!(hit.node_id, mapped(&split, 13, "beta"));
}

#[test]
fn stub_replicas_and_primaries_map_back_to_one_origin() {
    let source = source();
    let split = build_split(&plan_for(&source), &source).unwrap();
    let origins = Origins::new(&split);
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
}

#[tokio::test]
async fn other_projects_hits_are_removed_and_none_leak() {
    let report = Run::new().go().await.unwrap();
    for (project, summary) in &report.projects {
        assert_eq!(summary.leaked, 0, "{project}");
        assert_eq!(summary.layers, vec!["general".to_string()]);
    }

    // Asked from alpha, beta's own ruling was a before hit and is gone after.
    let q = query(&report, "alpha", "beta parser is strict");
    assert!(q.before.iter().any(|h| h.origin == id(12)));
    assert!(q.removed_foreign.contains(&id(12)));
    assert!(!q.after.iter().any(|h| h.origin == id(12)));
    assert!(q.after.iter().all(|h| h.primary != "beta" || h.hub));
    assert!(report.projects["alpha"].removed_foreign_by_primary["beta"] >= 1);

    // The operator ruling answers from the general layer, for both projects.
    for project in ["alpha"] {
        let q = query(&report, project, "operator-wide review conventions");
        let hit = q.after.iter().find(|h| h.origin == id(17)).unwrap();
        assert_eq!(hit.session, "general");
        assert!(!q.lost.contains(&id(17)));
    }
    assert!(report.projects["alpha"].after_by_session["general"] >= 1);
    assert_eq!(
        report.layer_knobs["alpha"],
        vec![("general".into(), 0.5, 0.40)]
    );
}

#[tokio::test]
async fn without_layers_general_is_out_of_scope() {
    let mut run = Run::new();
    run.stack = vec![];
    let report = run.go().await.unwrap();
    let q = query(&report, "alpha", "operator-wide review conventions");
    assert!(q.removed_foreign.contains(&id(17)));
    assert!(q.after.iter().all(|h| h.session == "alpha"));
    assert!(report.projects["alpha"].layers.is_empty());
    assert!(report.layer_knobs.is_empty());
}

#[test]
fn a_hit_outside_its_origins_placements_is_counted_as_a_leak() {
    let source = source();
    let split = build_split(&plan_for(&source), &source).unwrap();
    let origins = Origins::new(&split);
    let empty = LayeredRecall::single(&SessionId::from("x"), DetailedRecall::warn_only("w".into()));
    let hit = |origin: u64, session: &str| VerifyHit {
        rank: 1,
        origin: id(origin),
        session: session.into(),
        node_id: id(origin),
        primary: "beta".into(),
        hub: false,
        included_in_context: true,
        annotations: vec![],
        also: vec![],
    };
    let visible = BTreeSet::from(["alpha".to_string(), "general".to_string()]);
    // Beta's ruling (12) answering from alpha would be a leak.
    let q = compare(
        "q",
        &empty,
        &empty,
        vec![hit(12, "mixed")],
        vec![hit(12, "alpha")],
        &visible,
        &origins,
    );
    assert_eq!(q.leaked, vec![id(12)]);
    assert_eq!(q.removed_foreign, vec![id(12)]);
    assert_eq!(q.recall_at_k, None);
}

async fn refusal(run: &Run) -> String {
    run.go().await.unwrap_err().to_string()
}

#[tokio::test]
async fn mismatched_outputs_contracts_and_query_sets_are_refused() {
    let mut run = Run::new();
    run.targets
        .get_mut("alpha")
        .unwrap()
        .concepts
        .first_mut()
        .unwrap()
        .content
        .push_str(" (edited)");
    assert!(refusal(&run).await.contains("target alpha does not match"));

    let mut run = Run::new();
    run.targets.remove("beta");
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
    assert!(
        refusal(&run).await.contains("session mixed"),
        "{}",
        refusal(&run).await
    );

    let mut run = Run::new();
    run.top_k = 0;
    assert!(refusal(&run).await.contains("top_k"));

    // A plan for another source: the D2 build refuses it before any replay.
    let mut other = source();
    other.interactions[0].prompt_text = Some("changed".into());
    let plan = plan_for(&source());
    assert!(build_split(&plan, &other).is_err());
}

#[tokio::test]
async fn layered_dedupe_is_not_a_loss() {
    // In the unmodified fixture every 2-d holder vector is nearly collinear,
    // so the general ruling's copy is merged into a project hit by layered
    // dedupe: the memory is still answered, as that hit's `also`.
    let mut run = Run::new();
    run.source = source();
    let split = build_split(&plan_for(&run.source), &run.source).unwrap();
    run.targets = targets(&split);
    let report = run.go().await.unwrap();
    let q = query(&report, "alpha", "operator-wide review conventions");
    assert!(q.deduped.contains(&id(17)), "{q:#?}");
    assert!(!q.lost.contains(&id(17)));
    assert!(q
        .after
        .iter()
        .any(|h| h.also.contains(&("general".to_string(), id(17)))));
    assert!(report.projects["alpha"].deduped >= 1);
}
