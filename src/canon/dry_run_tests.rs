//! Tests for the read-only dry run and the alias preview (#36 PR 5), on the
//! shared fair-test fixture. Nothing here reads a private store.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::*;
use crate::canon::alias_preview::{alias_preview, render_alias_preview, FlipRole, PairInput};
use crate::daemon::score::rescore;
use crate::test_util::fair_test as fair;
use crate::types::ScoreTable;
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
        // The four #36 rulings and #20's two beliefs (supersession is off).
        assert_eq!(report.canonical_total, 6);
        let canonical: HashSet<NodeId> = [
            "lease_stays",
            "wedge",
            "identity",
            "async_ack",
            "retry_forever",
            "ttl_fixed",
        ]
        .map(fair::concept_id)
        .into_iter()
        .collect();
        let rulings = canonical.clone();
        let canonical: HashSet<NodeId> = report.canonical.iter().map(|r| r.id).collect();
        assert_eq!(canonical, rulings);
        assert!(report.canonical.iter().all(|r| r.dependents == 6));
        // Six Canonicals, all rulings: no Resource reached Canonical.
        assert_eq!(report.reached["Canonical"].total, 6);
        assert_eq!(report.reached["Canonical"].resource, 0);
        // The alias is the one Venerable that is not a ruling, wedge_five or
        // one of #20's two contradictors.
        assert_eq!(report.reached["Venerable"].total, 10);
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
        // Review M4: the oracle waits far longer than the dry run's own
        // stop rule (200 quiet cycles), so an early stop would show here.
        let oracle = fair::fixed_point(&sqlite, snap.clone(), &p.eval, 200).await;
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
        // The other four supported rulings (two of them #20's) wait at
        // Venerable, and the explanation names the budget.
        assert_eq!(
            a.fixed_point
                .one_gate_away
                .get("Venerable->Canonical stage3/canonical_budget")
                .map(|s| s.total),
            Some(4)
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
        assert_eq!(d.reached["Canonical"], (6, 5));
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
        assert_eq!(p.delta.reached["Venerable"], (10, 9));
        assert_eq!(p.delta.reached["Canonical"], (6, 6));
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

/// Review M4: queues longer than one batch. With a batch of 1 or 2 the
/// fixture's 7 to 8 first-cycle Candidates and 6 Venerables each take
/// several cycles to go round, so a stop rule that quit before the queues
/// drained would end short of the fixed point the oracle (200 quiet cycles,
/// a real SQLite store) reaches.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn queues_longer_than_a_batch_still_reach_the_oracles_fixed_point() {
    use crate::store::SqliteStore;
    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    for policy in BOTH {
        for batch in [1, 2] {
            let mut p = params(policy);
            p.eval.batch_size = batch;
            p.min_quiet_cycles = 1;
            let sqlite = SqliteStore::connect("sqlite::memory:").unwrap();
            sqlite.init_schema().await.unwrap();
            sqlite.seed(&snap).await.unwrap();
            let oracle = fair::fixed_point(&sqlite, snap.clone(), &p.eval, 200).await;
            let (report, finals) = run(&snap, &p).await.unwrap();
            all_held(&report);
            assert!(report.quiet_needed > 1, "the ring term must bind");
            for c in oracle.concepts() {
                assert_eq!(
                    finals[&c.id], c.canonization_status,
                    "{policy:?} batch {batch}: {}",
                    c.id
                );
            }
            fair::assert_expected(&by_key(&finals), policy, "small batches");
        }
    }
}

/// Review M4: the production loop itself. `CanonizationTask` runs on the
/// same snapshot with an injected clock (61 s a cycle), woken cycle by
/// cycle, with the daemon's scores recomputed from `Config::scoring`
/// before each, until nothing changes for 100 cycles; its fixed point must
/// equal the dry run's.
#[tokio::test]
async fn the_canonization_task_reaches_the_same_fixed_point() {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    use crate::canon::CanonizationTask;
    use crate::store::MemoryStore;

    let mut snap = fair::snapshot(true);
    prepare(&mut snap, false);
    let config = crate::Config::default();
    for policy in BOTH {
        let mut p = params(policy);
        p.eval.batch_size = 2;
        p.weights = config.scoring;
        let (_, finals) = run(&snap, &p).await.unwrap();

        let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
        let store = Arc::new(MemoryStore::new());
        store.seed(snap.clone()).unwrap();
        let score = |g: &Graph| ScoreTable {
            epoch: g.epoch(),
            ranked: rescore(g, &config.scoring),
        };
        let scores = Arc::new(RwLock::new(score(&graph.read())));
        let (tx, _rx) = crate::daemon::events::event_channel();
        let millis = Arc::new(AtomicI64::new(p.now.timestamp_millis()));
        let clock = millis.clone();
        let task = CanonizationTask::new(
            graph.clone(),
            store,
            scores.clone(),
            tx,
            p.eval.clone(),
            Duration::from_secs(3_600),
        )
        .with_clock(Arc::new(move || {
            DateTime::from_timestamp_millis(clock.load(Ordering::SeqCst)).unwrap()
        }));
        let handle = task.spawn();
        let snapshot_of = |g: &Graph| -> FinalStatuses {
            g.concepts()
                .map(|c| (c.id, c.canonization_status))
                .collect()
        };
        let mut last = snapshot_of(&graph.read());
        let mut unchanged = 0;
        for cycle in 1..=600u64 {
            *scores.write() = score(&graph.read());
            task.wake();
            for _ in 0..5_000 {
                if task.cycles() >= cycle {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            assert_eq!(task.cycles(), cycle, "cycle {cycle} did not run");
            millis.fetch_add(61_000, Ordering::SeqCst);
            let now = snapshot_of(&graph.read());
            if now == last {
                unchanged += 1;
                if unchanged >= 100 {
                    break;
                }
            } else {
                unchanged = 0;
                last = now;
            }
        }
        handle.abort();
        assert_eq!(task.failures(), 0);
        assert!(unchanged >= 100, "{policy:?}: the task never settled");
        assert_eq!(
            last, finals,
            "{policy:?}: the task and the dry run disagree"
        );
    }
}

/// Review M1: a clock earlier than the data fails `clock_after_the_data`
/// and counts what it would leave out.
#[tokio::test]
async fn a_clock_before_the_data_fails_and_counts_the_rows_it_leaves_out() {
    let snap = fair::snapshot(true);
    let ok = dry_run_all(
        snap.clone(),
        &[PromotionPolicy::Swarm],
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    assert_eq!(ok.rows_after_clock.total(), 0);
    assert!(ok
        .invariants
        .iter()
        .any(|i| i.name == "clock_after_the_data" && i.held));
    // Day 10 of the fixture's 19: about half the rows are later.
    let mut early = params(PromotionPolicy::Swarm);
    early.now = fair::now() - chrono::Duration::days(11);
    let r = dry_run_all(snap.clone(), &[PromotionPolicy::Swarm], &early)
        .await
        .unwrap();
    let clock = r
        .invariants
        .iter()
        .find(|i| i.name == "clock_after_the_data")
        .unwrap();
    assert!(!clock.held);
    assert!(r.rows_after_clock.interactions > 0 && r.rows_after_clock.edges > 0);
    assert!(clock.detail.contains("pin --at at or after"));
    let a = alias_preview(snap, &PairInput::Detect, &[PromotionPolicy::Swarm], &early)
        .await
        .unwrap();
    assert!(a
        .invariants
        .iter()
        .any(|i| i.name == "clock_after_the_data" && !i.held));
}

/// Review M2: the run without redirects must hold its invariants too, and
/// no flip is computed against a run that never reached its fixed point.
#[tokio::test]
async fn flips_need_both_runs_to_reach_their_fixed_point() {
    let snap = fair::snapshot(true);
    let map = format!(
        "{} {}",
        fair::concept_id("lease_alias").0,
        fair::concept_id("lease_stays").0
    );
    let mut p = params(PromotionPolicy::Swarm);
    p.max_cycles = 2;
    let r = alias_preview(snap, &PairInput::Map(map), &[PromotionPolicy::Swarm], &p)
        .await
        .unwrap();
    let both = r
        .invariants
        .iter()
        .find(|i| i.name == "both_runs_hold_their_invariants")
        .unwrap();
    assert!(!both.held);
    assert!(
        both.detail.contains("without redirects: fixed_point"),
        "{}",
        both.detail
    );
    assert!(r.policies[0].flips.is_empty());
}

/// Review H2 (library half): a reviewed map with a refused line fails
/// `every_reviewed_line_accepted`; detection's refusals never do.
#[tokio::test]
async fn a_refused_reviewed_line_fails_the_preview_and_a_detected_one_does_not() {
    let snap = fair::snapshot(true);
    let map = format!(
        "{} {}\nnot a pair at all\n",
        fair::concept_id("lease_alias").0,
        fair::concept_id("lease_stays").0
    );
    let p = params(PromotionPolicy::Swarm);
    let r = alias_preview(
        snap.clone(),
        &PairInput::Map(map),
        &[PromotionPolicy::Swarm],
        &p,
    )
    .await
    .unwrap();
    let line = r
        .invariants
        .iter()
        .find(|i| i.name == "every_reviewed_line_accepted")
        .unwrap();
    assert!(!line.held);
    let d = alias_preview(snap, &PairInput::Detect, &[PromotionPolicy::Swarm], &p)
        .await
        .unwrap();
    assert!(d
        .invariants
        .iter()
        .all(|i| i.name != "every_reviewed_line_accepted"));
}

/// Review M3: a redirected alias that already holds a status keeps it, and
/// the report says so.
#[tokio::test]
async fn an_alias_keeping_its_status_is_reported() {
    let mut snap = fair::snapshot(true);
    let alias = fair::concept_id("lease_alias");
    for c in snap.concepts.iter_mut().filter(|c| c.id == alias) {
        c.canonization_status = CanonizationStatus::Venerable;
    }
    let map = format!("{} {}", alias.0, fair::concept_id("lease_stays").0);
    let r = alias_preview(
        snap,
        &PairInput::Map(map),
        &[PromotionPolicy::Swarm],
        &params(PromotionPolicy::Swarm),
    )
    .await
    .unwrap();
    assert_eq!(r.redirects[0].alias_status, CanonizationStatus::Venerable);
    assert_eq!(r.aliases_keeping_status.get("Venerable"), Some(&1));
    let text = render_alias_preview(&r, false);
    assert!(
        text.contains("aliases keeping status Venerable with no citations left: 1"),
        "{text}"
    );
    assert!(
        text.contains(&format!("{alias} Entity Venerable ->")),
        "{text}"
    );
}

/// Review L2: every Canonical id is stored and compared, not only the
/// listed rows; a baseline of another session or version is refused.
#[tokio::test]
async fn every_canonical_id_is_compared_and_a_foreign_baseline_is_refused() {
    let snap = fair::snapshot(true);
    let mut p = params(PromotionPolicy::Swarm);
    p.top = 1;
    let r = dry_run_all(snap.clone(), &[PromotionPolicy::Swarm], &p)
        .await
        .unwrap();
    assert_eq!(r.policies[0].canonical.len(), 1);
    assert_eq!(r.policies[0].canonical_ids.len(), 6);
    let mut fewer = snap.clone();
    let rulings = ["lease_stays", "wedge", "identity", "async_ack"].map(fair::concept_id);
    // Drop the citations of whichever ruling is not the one listed row.
    let unlisted = *rulings
        .iter()
        .find(|id| **id != r.policies[0].canonical[0].id)
        .unwrap();
    fewer
        .edges
        .retain(|e| !(e.edge_type == EdgeType::Dependency && e.target == unlisted));
    let after = dry_run_all(fewer, &[PromotionPolicy::Swarm], &p)
        .await
        .unwrap();
    assert_eq!(
        compare(&r, &after).policies[0].canonical_removed,
        vec![unlisted]
    );
    assert!(baseline_mismatch(&r, &after).is_none());
    let mut other = r.clone();
    other.session = SessionId::from("another-session");
    assert!(baseline_mismatch(&other, &after)
        .unwrap()
        .contains("session"));
    let mut old = r.clone();
    old.version = REPORT_VERSION - 1;
    assert!(baseline_mismatch(&old, &after).unwrap().contains("version"));
}

/// Review L3: an error keeps its ids and withholds the rest; a non-id
/// `--watch` argument is not echoed.
#[test]
fn errors_keep_ids_and_withhold_text() {
    let id = fair::concept_id("wedge");
    let r = redact(&format!(
        "canonical key 'flush wedg write queue' held by {id} and more"
    ));
    assert!(r.contains(&id.to_string()));
    assert!(!r.contains("wedg"));
    assert!(!redact("key 'secret text' duplicated").contains("secret"));
    let e = resolve_watch(&fair::snapshot(true), &["no flush may wedge".to_string()]).unwrap_err();
    assert!(!e.contains("wedge"), "{e}");
}
