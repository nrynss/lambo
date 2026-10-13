//! Tests for the read-only dry run and the alias preview (#36 PR 5), on the
//! shared fair-test fixture. Nothing here reads a private store.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::*;
use crate::canon::alias_preview::{alias_preview, render_alias_preview, FlipRole, PairInput};
use crate::test_util::fair_test as fair;
use crate::types::{EdgeType, Node};

const BOTH: [PromotionPolicy; 2] = [PromotionPolicy::Swarm, PromotionPolicy::Solo];

fn params(policy: PromotionPolicy) -> DryRunParams {
    let mut p = DryRunParams::new(policy, fair::now());
    p.min_quiet_cycles = 5;
    p.max_cycles = 500;
    p
}

fn by_key(finals: &FinalStatuses) -> HashMap<&'static str, CanonizationStatus> {
    fair::CONCEPTS
        .iter()
        .map(|(k, ..)| (*k, finals[&fair::concept_id(k)]))
        .collect()
}

fn all_held(p: &PolicyReport) {
    for i in &p.invariants {
        assert!(i.held, "{:?}: {} failed ({})", p.policy, i.name, i.detail);
    }
}

#[tokio::test]
async fn the_dry_run_reaches_the_fair_test_fixed_point_under_both_policies() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    for policy in BOTH {
        let (report, finals) = run(&snap, &params(policy)).await.unwrap();
        fair::assert_expected(&by_key(&finals), policy, "the dry-run overlay");
        all_held(&report);
        assert_eq!(report.stop, StopReason::FixedPoint);
        assert_eq!(report.canonical_total, 4);
        let canonical: HashSet<NodeId> = report.canonical.iter().map(|r| r.id).collect();
        let rulings: HashSet<NodeId> = ["lease_stays", "wedge", "identity", "async_ack"]
            .map(fair::concept_id)
            .into_iter()
            .collect();
        assert_eq!(canonical, rulings);
        assert!(report.canonical.iter().all(|r| r.dependents == 6));
        // Four Canonicals, all rulings: no Resource reached Canonical.
        assert_eq!(report.reached["Canonical"].total, 4);
        assert_eq!(report.reached["Canonical"].resource, 0);
        // The alias is the one Venerable that is not a ruling or wedge_five.
        assert_eq!(report.reached["Venerable"].total, 6);
        assert_eq!(report.demoted.total, 0);
    }
}

/// The independent oracle: the evaluator driven by `fair::fixed_point` (its
/// own loop and stop rule) on a real SQLite store, not the overlay's
/// MemoryStore, and the Stage 1 floor recounted from the raw edges.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn the_dry_run_agrees_with_an_independent_oracle() {
    use crate::store::SqliteStore;
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    for policy in BOTH {
        let sqlite = SqliteStore::connect("sqlite::memory:").unwrap();
        sqlite.init_schema().await.unwrap();
        sqlite.seed(&snap).await.unwrap();
        let p = params(policy);
        let oracle = fair::fixed_point(&sqlite, snap.clone(), &p.eval, 10).await;
        let (report, finals) = run(&snap, &p).await.unwrap();
        for c in oracle.concepts() {
            assert_eq!(
                finals[&c.id], c.canonization_status,
                "{policy:?}: {} differs from the SQLite oracle",
                c.id
            );
        }
        let canonical_oracle = oracle
            .concepts()
            .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
            .count();
        assert_eq!(report.canonical_total, canonical_oracle);

        // Stage 1 under Swarm, recounted by hand: survival >= 3 and declared
        // dependents (Dependency sources in, parent_of children out) from
        // at least two interactions other than the concept's own.
        if policy == PromotionPolicy::Swarm {
            let origin: HashMap<NodeId, NodeId> = snap
                .concepts
                .iter()
                .map(|c| (c.id, c.origin_interaction))
                .collect();
            let mut others: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            for e in &snap.edges {
                let (pillar, dependent) = match e.edge_type {
                    EdgeType::Dependency => (e.target, e.source),
                    EdgeType::Hierarchical => (e.source, e.target),
                    _ => continue,
                };
                if pillar == dependent {
                    continue;
                }
                if let (Some(&own), Some(&from)) = (origin.get(&pillar), origin.get(&dependent))
                    && own != from
                {
                    others.entry(pillar).or_default().insert(from);
                }
            }
            let hand = snap
                .concepts
                .iter()
                .filter(|c| c.gc_survived >= 3 && others.get(&c.id).is_some_and(|s| s.len() >= 2))
                .count();
            let eligible = report
                .first_evaluation
                .eligible
                .get("None->Candidate")
                .map_or(0, |s| s.total);
            assert_eq!(eligible, hand, "Stage 1 floor recounted from the edges");
        }
    }
}

#[tokio::test]
async fn a_full_budget_stops_deterministically() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    for policy in BOTH {
        let mut p = params(policy);
        p.eval.max_canonical_nodes = 2;
        let (a, fa) = run(&snap, &p).await.unwrap();
        let (b, fb) = run(&snap, &p).await.unwrap();
        assert_eq!(a, b, "{policy:?}: two runs over one copy differ");
        assert_eq!(fa, fb);
        all_held(&a);
        assert_eq!(a.stop, StopReason::FixedPoint);
        assert!(a.budget_saturated);
        assert_eq!(a.canonical_total, 2);
        // The other two supported rulings wait at Venerable, and the
        // explanation names the budget.
        assert_eq!(
            a.fixed_point
                .one_gate_away
                .get("Venerable->Canonical stage3/canonical_budget")
                .map(|s| s.total),
            Some(2)
        );
    }
}

#[tokio::test]
async fn a_running_cooldown_is_waited_out_before_the_run_stops() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    let cooldown = Duration::from_secs(3_600);
    let ruling = fair::concept_id("lease_stays");
    for c in snap.concepts.iter_mut().filter(|c| c.id == ruling) {
        c.last_demotion_time = Some(fair::now() - chrono::Duration::seconds(10));
    }
    for policy in BOTH {
        let mut p = params(policy);
        p.eval.cooldown = cooldown;
        p.min_quiet_cycles = 3;
        let g = Graph::from_snapshot(snap.clone()).unwrap();
        // Without the cooldown rule three empty cycles would end the run
        // with the ruling still waiting at Venerable.
        assert_eq!(quiet_needed(&g, &p), 3);
        assert!(cooling(&g, &p, p.now));
        let (a, fa) = run(&snap, &p).await.unwrap();
        let (b, _) = run(&snap, &p).await.unwrap();
        assert_eq!(a, b);
        all_held(&a);
        assert_eq!(fa[&ruling], CanonizationStatus::Canonical, "{policy:?}");
        assert!(a.cycles as u64 * 61 >= 3_590, "{} cycles", a.cycles);
    }
}

#[tokio::test]
async fn the_cycle_limit_fails_the_fixed_point_invariant() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    let mut p = params(PromotionPolicy::Swarm);
    p.max_cycles = 1;
    let r = dry_run(&snap, &p).await.unwrap();
    assert_eq!((r.stop, r.cycles), (StopReason::CycleLimit, 1));
    let fixed = r
        .invariants
        .iter()
        .find(|i| i.name == "fixed_point")
        .unwrap();
    assert!(!fixed.held);
}

#[tokio::test]
async fn the_snapshot_is_never_modified() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    let before = snap.clone();
    let _ = dry_run_all(snap.clone(), &BOTH, &params(PromotionPolicy::Swarm))
        .await
        .unwrap();
    assert_eq!(snap, before);
}

fn assert_no_text(out: &str) {
    for (key, _, content, _) in fair::CONCEPTS {
        // Single-word labels ("write path") could collide with type names;
        // every fixture content is longer than any label printed.
        assert!(!out.contains(content), "{key}'s text leaked: {content}");
    }
    for (key, .., prompt) in fair::INTERACTIONS {
        assert!(!out.contains(prompt), "interaction {key}'s prompt leaked");
    }
}

#[tokio::test]
async fn reports_carry_ids_and_labels_never_concept_text() {
    let snap = fair::snapshot(true);
    let r = dry_run_all(snap.clone(), &BOTH, &params(PromotionPolicy::Swarm))
        .await
        .unwrap();
    assert_no_text(&render_report(&r));
    assert_no_text(&serde_json::to_string(&r).unwrap());
    let map = format!(
        "{} {}",
        fair::concept_id("lease_alias").0,
        fair::concept_id("lease_stays").0
    );
    let a = alias_preview(
        snap,
        &PairInput::Map(map),
        &BOTH,
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    assert_no_text(&render_alias_preview(&a, true));
    assert_no_text(&serde_json::to_string(&a).unwrap());
}

/// The fixture's text report is pinned: a change to what the dry run counts
/// or how it prints shows up here. Regenerate with
/// `LAMBO_UPDATE_GOLDEN=1 cargo test --features store-memory,fixtures --lib canon::dry_run::tests::the_fixture_report_is_pinned`.
#[tokio::test]
async fn the_fixture_report_is_pinned() {
    let r = dry_run_all(fair::snapshot(true), &BOTH, &params(PromotionPolicy::Swarm))
        .await
        .unwrap();
    let text = render_report(&r);
    let path = crate::fixtures::fixture_path("canon-dry-run-fair-test").with_extension("txt");
    if std::env::var_os("LAMBO_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &text).expect("write golden");
    }
    let committed = std::fs::read_to_string(&path).expect("golden present");
    assert_eq!(committed, text, "{path:?} drifted");
}

#[tokio::test]
async fn a_report_compares_with_its_baseline() {
    let snap = fair::snapshot(true);
    let r = dry_run_all(snap.clone(), &BOTH, &params(PromotionPolicy::Swarm))
        .await
        .unwrap();
    // A report survives its own JSON (the `--baseline` file).
    let back: DryRunReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(back, r);
    let same = compare(&r, &r);
    assert_eq!(same.policies.len(), 2);
    for d in &same.policies {
        assert!(d.canonical_added.is_empty() && d.canonical_removed.is_empty());
        assert!(d.reached.values().all(|(b, a)| b == a));
    }
    // Take one supported ruling's citations away: the comparison names it.
    let mut fewer = snap.clone();
    let wedge = fair::concept_id("wedge");
    fewer
        .edges
        .retain(|e| !(e.edge_type == EdgeType::Dependency && e.target == wedge));
    let after = dry_run_all(fewer, &BOTH, &params(PromotionPolicy::Swarm))
        .await
        .unwrap();
    let c = compare(&r, &after);
    for d in &c.policies {
        assert_eq!(d.canonical_removed, vec![wedge]);
        assert_eq!(d.reached["Canonical"], (4, 3));
    }
    assert!(render_comparison(&c).contains(&format!("canonical removed: {wedge}")));
}

#[tokio::test]
async fn the_alias_preview_flips_the_fixture_alias() {
    let snap = fair::snapshot(true);
    let alias = fair::concept_id("lease_alias");
    let ruling = fair::concept_id("lease_stays");
    let map = format!("{} {}\n", alias.0, ruling.0);
    let r = alias_preview(
        snap.clone(),
        &PairInput::Map(map),
        &BOTH,
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    for i in &r.invariants {
        assert!(i.held, "{} ({})", i.name, i.detail);
    }
    assert_eq!((r.edges_retired, r.edges_added), (3, 3));
    for p in &r.policies {
        // The known defect: the name was Venerable on citations meant for the
        // ruling. With them moved, the name has no dependents and stays None;
        // the ruling stays Canonical and now carries nine dependents.
        let flip = p
            .flips
            .iter()
            .find(|f| f.id == alias)
            .expect("the alias flips");
        assert_eq!(flip.role, FlipRole::Alias);
        assert_eq!(
            (flip.before, flip.after),
            (CanonizationStatus::Venerable, CanonizationStatus::None)
        );
        assert_eq!(p.flips_total, 1, "{:?}: {:?}", p.policy, p.flips);
        let lease = p.after.canonical.iter().find(|c| c.id == ruling).unwrap();
        assert_eq!(lease.dependents, 9);
        assert_eq!(p.delta.reached["Venerable"], (6, 5));
        assert_eq!(p.delta.reached["Canonical"], (4, 4));
        all_held(&p.after);
    }
    // Detection proposes nothing on the fixture, so nothing flips.
    let d = alias_preview(
        snap,
        &PairInput::Detect,
        &BOTH,
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    assert!(d.redirects.is_empty());
    assert!(d.policies.iter().all(|p| p.flips_total == 0));
}

#[tokio::test]
async fn the_alias_preview_refuses_and_applies_nothing_for_a_bad_map() {
    let snap = fair::snapshot(true);
    let alias = fair::concept_id("lease_alias");
    let map = format!(
        "{a} {r1}\n{a} {r2}\n",
        a = alias.0,
        r1 = fair::concept_id("lease_stays").0,
        r2 = fair::concept_id("wedge").0
    );
    let r = alias_preview(
        snap,
        &PairInput::Map(map),
        &[PromotionPolicy::Swarm],
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    assert!(r.redirects.is_empty());
    assert_eq!(r.refusal_counts.get("Ambiguous"), Some(&2));
    assert_eq!((r.edges_retired, r.edges_added), (0, 0));
    assert_eq!(r.policies[0].flips_total, 0);
    let still = r.policies[0]
        .after
        .end
        .get("Venerable")
        .and_then(|t| t.get("Entity"))
        .copied();
    assert_eq!(still, Some(1), "the alias is still Venerable");
    // The concept is untouched in the overlay: the graph still holds it.
    let g = Graph::from_snapshot(fair::snapshot(true)).unwrap();
    assert!(matches!(g.node(alias), Some(Node::Concept(_))));
}

#[tokio::test]
async fn watched_concepts_are_reported_by_id_at_the_start_and_the_stop() {
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    let short = |k: &str| fair::concept_id(k).0.simple().to_string()[..8].to_string();
    let refs = vec![
        short("wedge"),
        fair::concept_id("wedge_five").0.to_string(),
        short("identity_burst"),
    ];
    let watch = resolve_watch(&snap, &refs).unwrap();
    assert_eq!(
        watch,
        ["wedge", "wedge_five", "identity_burst"].map(fair::concept_id)
    );
    for bad in ["zzzzzzzz", "00000000", "writer lease"] {
        assert!(resolve_watch(&snap, &[bad.to_string()]).is_err(), "{bad}");
    }
    let mut p = params(PromotionPolicy::Swarm);
    p.watch = watch.clone();
    let r = dry_run(&snap, &p).await.unwrap();
    let ids = |rows: &[IdRow]| rows.iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(&r.first_evaluation.watched), watch);
    assert_eq!(ids(&r.fixed_point.watched), watch);
    let end = &r.fixed_point.watched;
    assert_eq!(end[0].status, CanonizationStatus::Canonical);
    assert_eq!(
        (end[1].status, end[1].dependents, end[1].unmet.clone()),
        (
            CanonizationStatus::Venerable,
            5,
            vec!["stage3/blast_radius".to_string()]
        )
    );
    assert_eq!(end[2].status, CanonizationStatus::Candidate);
    assert!(end[2].unmet.contains(&"stage2/coverage".to_string()));
    assert!(render_report(
        &dry_run_all(snap, &[PromotionPolicy::Swarm], &p)
            .await
            .unwrap()
    )
    .contains(&format!("watched {}", fair::concept_id("wedge_five"))));
}
