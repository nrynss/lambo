//! The canonization fair test (#36 PR 3) on the shared fixture
//! `fixtures/session-fair-test.json` (`crate::test_util::fair_test`).
//!
//! Both policies run the real evaluator from the fixture's all-`None` start
//! to a fixed point, on `MemoryStore` and on SQLite, and must promote the
//! four supported rulings to Canonical and leave every under-supported
//! variant and every piece of chaff below Canonical, exactly as
//! `fair_test::expected` lists. The fixture's one `Contradicts` edge must
//! change nothing.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::canon::{
    admission_inputs, explain_admission, render_admission, EvalParams, PromotionPolicy,
};
use crate::config::ScoringWeights;
use crate::daemon::score::rescore;
use crate::graph::Graph;
use crate::store::{GraphStore, MemoryStore};
use crate::test_util::fair_test as fair;
use crate::types::{CanonizationStatus, EdgeType, GraphSnapshot, Node, NodeId, ScoreTable};

const POLICIES: [PromotionPolicy; 2] = [PromotionPolicy::Swarm, PromotionPolicy::Solo];

fn params(policy: PromotionPolicy) -> EvalParams {
    EvalParams {
        promotion_policy: policy,
        ..EvalParams::default()
    }
}

async fn memory_store(snap: &GraphSnapshot) -> MemoryStore {
    let store = MemoryStore::new();
    store.flush(&fair::batch_of(snap), None).await.unwrap();
    store
}

fn snapshot() -> GraphSnapshot {
    crate::fixtures::load_snapshot(fair::NAME).unwrap()
}

/// The committed JSON is exactly what `fair_test` builds. Regenerate with
/// `LAMBO_UPDATE_GOLDEN=1` after a deliberate change to the tables.
#[test]
fn fair_test_fixture_is_current() {
    let built = serde_json::to_value(fair::snapshot(true)).unwrap();
    let path = crate::fixtures::fixture_path(fair::NAME);
    if std::env::var_os("LAMBO_UPDATE_GOLDEN").is_some() {
        let mut text = serde_json::to_string_pretty(&built).unwrap();
        text.push('\n');
        std::fs::write(&path, text).expect("write fixture");
    }
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture present")).unwrap();
    assert_eq!(
        committed, built,
        "{path:?} drifted from test_util::fair_test"
    );
}

/// The fixture's premises, measured through the stores' own queries rather
/// than restated: no seeded status, no outbound `Dependency` from any
/// ruling (so no reciprocal trick), each supported ruling has six aged
/// dependents from at least three other interactions with enough spread to
/// clear Stage 2 under the 14-day cap, the session is longer than the cap,
/// and there is exactly one `Contradicts` edge.
#[tokio::test]
async fn the_fixture_is_real_shaped() {
    let snap = snapshot();
    assert!(snap
        .concepts
        .iter()
        .all(|c| c.canonization_status == CanonizationStatus::None && c.gc_survived >= 3));
    assert!(snap.canonization_events.is_empty());
    // #36 PR 3 review L6: every edge id is the hash of its natural key, so
    // rows #20 inserts anywhere leave existing ids alone; and ids are unique.
    for e in &snap.edges {
        assert_eq!(
            e.id,
            fair::hashed_id(&format!(
                "edge/{}/{}/{:?}",
                e.source.0, e.target.0, e.edge_type
            ))
        );
    }
    let mut ids: Vec<NodeId> = snap.edges.iter().map(|e| e.id).collect();
    ids.extend(snap.concepts.iter().map(|c| c.id));
    ids.extend(snap.interactions.iter().map(|i| i.id));
    let n = ids.len();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), n, "fixture ids collide");
    let rulings = [
        "lease_stays",
        "wedge",
        "identity",
        "async_ack",
        "wedge_five",
        "identity_burst",
        "ack_single",
        "lease_self",
    ];
    for r in rulings {
        let id = fair::concept_id(r);
        assert!(
            !snap
                .edges
                .iter()
                .any(|e| e.source == id && e.edge_type == EdgeType::Dependency),
            "{r} must not depend on anything"
        );
    }
    let deps: Vec<(NodeId, NodeId)> = snap
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Dependency)
        .map(|e| (e.source, e.target))
        .collect();
    for (s, t) in &deps {
        assert!(
            !deps.contains(&(*t, *s)),
            "reciprocal Dependency {s} <-> {t}"
        );
    }
    assert_eq!(
        snap.edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Contradicts)
            .count(),
        1
    );
    let times: Vec<DateTime<Utc>> = snap.interactions.iter().map(|i| i.about_time()).collect();
    let extent = *times.iter().max().unwrap() - *times.iter().min().unwrap();
    assert!(extent > chrono::Duration::days(14), "{extent}");

    let store = memory_store(&snap).await;
    let sid = fair::session();
    let age = EvalParams::default().min_age;
    for r in ["lease_stays", "wedge", "identity", "async_ack"] {
        let id = fair::concept_id(r);
        let blast = store
            .blast_radius(&sid, id, age, fair::now())
            .await
            .unwrap();
        let span = store
            .interaction_span_with_window(
                &sid,
                id,
                age,
                fair::now(),
                crate::store::DEFAULT_COVERAGE_WINDOW,
            )
            .await
            .unwrap();
        assert_eq!(blast, 6, "{r}: six dependents");
        assert!(span.distinct >= 3, "{r}: {span:?}");
        assert!(span.coverage >= 0.3, "{r}: {span:?}");
    }
}

/// The acceptance test of record: both policies, from no seeded status,
/// promote exactly the four supported rulings to Canonical and leave every
/// variant and every piece of chaff where `fair_test::expected` says, on
/// `MemoryStore`.
#[tokio::test]
async fn both_policies_promote_the_supported_rulings_on_memory() {
    for policy in POLICIES {
        let snap = snapshot();
        let store = memory_store(&snap).await;
        let g = fair::fixed_point(&store, snap, &params(policy), 3).await;
        fair::assert_expected(&fair::statuses(&g), policy, "MemoryStore");
        for r in ["lease_stays", "wedge", "identity", "async_ack"] {
            match g.node(fair::concept_id(r)) {
                Some(Node::Concept(c)) => assert_eq!(c.blast_radius, Some(6), "{r}"),
                _ => unreachable!(),
            }
        }
    }
}

/// The same on SQLite.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn both_policies_promote_the_supported_rulings_on_sqlite() {
    for policy in POLICIES {
        let snap = snapshot();
        let store = crate::store::SqliteStore::connect("sqlite::memory:").unwrap();
        store.init_schema().await.unwrap();
        store.flush(&fair::batch_of(&snap), None).await.unwrap();
        let g = fair::fixed_point(&store, snap, &params(policy), 3).await;
        fair::assert_expected(&fair::statuses(&g), policy, "SQLite");
    }
}

/// #19 acceptance: the fixture's one contradiction changes nothing. With
/// and without it, both policies reach the same statuses, and every
/// concept's Stage 1 count, blast radius and span agree.
#[tokio::test]
async fn the_contradiction_changes_nothing() {
    let with = fair::snapshot(true);
    let without = fair::snapshot(false);
    assert_eq!(with.edges.len(), without.edges.len() + 1);
    let (s_with, s_without) = (memory_store(&with).await, memory_store(&without).await);
    let sid = fair::session();
    let p = EvalParams::default();
    let (g_with, g_without) = (
        Graph::from_snapshot(with.clone()).unwrap(),
        Graph::from_snapshot(without.clone()).unwrap(),
    );
    for (key, ..) in fair::CONCEPTS {
        let id = fair::concept_id(key);
        assert_eq!(
            crate::canon::dependent_interactions(&g_with, id, p.min_age, fair::now()),
            crate::canon::dependent_interactions(&g_without, id, p.min_age, fair::now()),
            "{key} dependent interactions"
        );
        assert_eq!(
            s_with
                .blast_radius(&sid, id, p.min_edge_age, fair::now())
                .await
                .unwrap(),
            s_without
                .blast_radius(&sid, id, p.min_edge_age, fair::now())
                .await
                .unwrap(),
            "{key} blast"
        );
        assert_eq!(
            s_with
                .interaction_span_with_window(&sid, id, p.min_age, fair::now(), p.coverage_window)
                .await
                .unwrap(),
            s_without
                .interaction_span_with_window(&sid, id, p.min_age, fair::now(), p.coverage_window)
                .await
                .unwrap(),
            "{key} span"
        );
    }
    for policy in POLICIES {
        let a = fair::fixed_point(&s_with, with.clone(), &params(policy), 3).await;
        let b = fair::fixed_point(&s_without, without.clone(), &params(policy), 3).await;
        assert_eq!(fair::statuses(&a), fair::statuses(&b), "{policy:?}");
        fair::assert_expected(
            &fair::statuses(&a),
            policy,
            "MemoryStore with the contradiction",
        );
    }
}

/// Design test: five dependents fail Stage 3 and six pass. `wedge_five`
/// stops at Venerable with five; one more cited from another interaction
/// takes it to Canonical, under both policies.
#[tokio::test]
async fn five_dependents_fail_and_six_pass() {
    for policy in POLICIES {
        let mut snap = snapshot();
        let store = memory_store(&snap).await;
        let g = fair::fixed_point(&store, snap.clone(), &params(policy), 3).await;
        assert_eq!(
            fair::statuses(&g)["wedge_five"],
            CanonizationStatus::Venerable
        );

        // A sixth: decision d06 (interaction w19) also cites wedge_five.
        let sixth = crate::types::Edge {
            event_time: None,
            id: NodeId(uuid::Uuid::from_u64_pair(0x3603_0004, 1)),
            session_id: fair::session(),
            source: fair::concept_id("d06"),
            target: fair::concept_id("wedge_five"),
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: fair::now() - chrono::Duration::days(1),
            last_reinforced: fair::now() - chrono::Duration::days(1),
        };
        snap.edges.push(sixth);
        let store = memory_store(&snap).await;
        let g = fair::fixed_point(&store, snap, &params(policy), 3).await;
        assert_eq!(
            fair::statuses(&g)["wedge_five"],
            CanonizationStatus::Canonical,
            "{policy:?}"
        );
    }
}

/// Design test: bounded probes stay fair. With one node per stage per cycle
/// the ring cursors and Stage 1's draining queue still reach every node, so
/// the fixed point is the unbounded one.
#[tokio::test]
async fn bounded_probes_stay_fair() {
    for policy in POLICIES {
        let snap = snapshot();
        let store = memory_store(&snap).await;
        let p = EvalParams {
            batch_size: 1,
            ..params(policy)
        };
        // A quiet stretch longer than any ring, so a pass over every member
        // has happened before the run stops.
        let g = fair::fixed_point(&store, snap, &p, fair::CONCEPTS.len() + 3).await;
        fair::assert_expected(&fair::statuses(&g), policy, "MemoryStore, batch 1");
    }
}

/// Design test at fixture scale: replayed reads preserve eligibility. Heavy
/// access on the chaff and the variants moves the composite (and so the
/// order) but not where anything ends.
#[tokio::test]
async fn replayed_reads_preserve_the_fixed_point() {
    for k in [1, 10, 100] {
        let mut snap = snapshot();
        for c in snap.concepts.iter_mut() {
            if !["lease_stays", "wedge", "identity", "async_ack"]
                .iter()
                .any(|r| fair::concept_id(r) == c.id)
            {
                c.access_count += k;
                c.last_accessed = Some(fair::now() - chrono::Duration::hours(1));
            }
        }
        for policy in POLICIES {
            let store = memory_store(&snap).await;
            let g = fair::fixed_point(&store, snap.clone(), &params(policy), 3).await;
            fair::assert_expected(&fair::statuses(&g), policy, &format!("MemoryStore, k={k}"));
        }
    }
}

/// Inspect's explanation agrees with the evaluator. From the all-`None`
/// start, a concept is explained eligible exactly when the policy's Stage 1
/// admits it; at the fixed point nothing is eligible, and each stopped row
/// names the gate that stopped it.
#[tokio::test]
async fn inspect_explains_every_outcome() {
    for policy in POLICIES {
        let p = params(policy);
        let snap = snapshot();
        let store = memory_store(&snap).await;
        let g = Graph::from_snapshot(snap.clone()).unwrap();
        let scores = ScoreTable {
            epoch: g.epoch(),
            ranked: rescore(&g, &ScoringWeights::default()),
        };
        let admitted = policy.scorer().candidates(&g, &scores, &p, fair::now());
        for (key, ..) in fair::CONCEPTS {
            let id = fair::concept_id(key);
            let inputs = admission_inputs(&g, id, &p, fair::now()).unwrap();
            let a = explain_admission(&store, &fair::session(), &inputs, &p, fair::now())
                .await
                .unwrap();
            assert_eq!(
                a.eligible,
                admitted.contains(&id),
                "{policy:?} {key}: {}",
                render_admission(&a)
            );
        }

        let end = fair::fixed_point(&store, snap, &p, 3).await;
        for (key, ..) in fair::CONCEPTS {
            let a = explain_at(&store, &end, key, &p).await;
            assert!(
                !a.eligible,
                "{policy:?} {key} eligible at the fixed point: {a:?}"
            );
        }
        let text = |a: &crate::canon::Admission| render_admission(a);
        let canonical = explain_at(&store, &end, "lease_stays", &p).await;
        assert!(
            text(&canonical).contains("no further promotion"),
            "{}",
            text(&canonical)
        );
        let five = explain_at(&store, &end, "wedge_five", &p).await;
        assert!(
            text(&five).contains("stage 3: 5 dependents, needs more than 5"),
            "{}",
            text(&five)
        );
        let burst = explain_at(&store, &end, "identity_burst", &p).await;
        assert!(
            text(&burst).contains("stage 2: coverage"),
            "{}",
            text(&burst)
        );
        let single = explain_at(&store, &end, "ack_single", &p).await;
        match policy {
            PromotionPolicy::Swarm => assert!(
                text(&single).contains("dependents from 1 other interactions, needs at least 2"),
                "{}",
                text(&single)
            ),
            PromotionPolicy::Solo => assert!(
                text(&single).contains("stage 2: dependents from 1 other interactions"),
                "{}",
                text(&single)
            ),
        }
        let file = explain_at(&store, &end, "file_flush", &p).await;
        assert!(!file.eligible, "{}", text(&file));
    }
}

async fn explain_at(
    store: &dyn GraphStore,
    g: &Graph,
    key: &str,
    p: &EvalParams,
) -> crate::canon::Admission {
    let inputs = admission_inputs(g, fair::concept_id(key), p, fair::now()).unwrap();
    explain_admission(store, &fair::session(), &inputs, p, fair::now())
        .await
        .unwrap()
}

/// The age floor reaches Stage 1 too: evaluated a minute after the last
/// citation of a ruling, with a floor longer than the whole fixture, nothing
/// is admitted under Swarm.
#[test]
fn an_age_floor_longer_than_the_fixture_admits_nothing() {
    let g = Graph::from_snapshot(snapshot()).unwrap();
    let p = EvalParams {
        min_age: Duration::from_secs(60 * 86_400),
        ..EvalParams::default()
    };
    let scores = ScoreTable::default();
    assert!(PromotionPolicy::Swarm
        .scorer()
        .candidates(&g, &scores, &p, fair::now())
        .is_empty());
}

/// Sonnet review HIGH: the live check runs the fixture under a fresh session
/// id. The same parameterised batch, under a non-default id, reaches the
/// expected fixed point on MemoryStore and SQLite, and shares no id with the
/// default fixture (the session name seeds every hash).
#[tokio::test]
async fn the_fixture_runs_under_another_session_id() {
    let sid = format!("{}-{}", fair::NAME, uuid::Uuid::new_v4());
    let snap = fair::snapshot_in(&sid, true);
    assert_eq!(snap.session_id.0, sid);
    let default = fair::snapshot(true);
    let ids = |s: &GraphSnapshot| -> std::collections::HashSet<NodeId> {
        s.interactions
            .iter()
            .map(|i| i.id)
            .chain(s.concepts.iter().map(|c| c.id))
            .chain(s.edges.iter().map(|e| e.id))
            .collect()
    };
    assert!(ids(&snap).is_disjoint(&ids(&default)), "ids collide");
    assert_eq!(ids(&snap).len(), ids(&default).len());
    for policy in POLICIES {
        let memory = memory_store(&snap).await;
        let g = fair::fixed_point(&memory, snap.clone(), &params(policy), 3).await;
        fair::assert_expected(&fair::statuses(&g), policy, "MemoryStore, fresh session");
        #[cfg(feature = "store-sqlite")]
        {
            let sqlite = crate::store::SqliteStore::connect("sqlite::memory:").unwrap();
            sqlite.init_schema().await.unwrap();
            sqlite.flush(&fair::batch_of(&snap), None).await.unwrap();
            let g = fair::fixed_point(&sqlite, snap.clone(), &params(policy), 3).await;
            fair::assert_expected(&fair::statuses(&g), policy, "SQLite, fresh session");
        }
    }
}
