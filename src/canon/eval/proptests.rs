//! Property tests: the apply phase of a canonization cycle against
//! `formal/LamboProofs/Canon.lean`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRng, TestRunner};

use super::*;
use crate::test_util::props::{self, cid};
use crate::types::{AgentId, Edge, EdgeType, Interaction};

use CanonizationStatus::{Candidate, Canonical, None as Plain, Venerable};

const STATUSES: [CanonizationStatus; 4] = [Plain, Candidate, Venerable, Canonical];

/// Lean `Status.rank` (`status_rank` in `src/canon/policy.rs`).
fn rank(s: CanonizationStatus) -> u8 {
    match s {
        Plain => 0,
        Candidate => 1,
        Venerable => 2,
        Canonical => 3,
    }
}

/// Lean `evalHop`: the four edges the eval loop takes.
fn eval_hop(a: CanonizationStatus, b: CanonizationStatus) -> bool {
    matches!(
        (a, b),
        (Plain, Candidate) | (Candidate, Venerable) | (Venerable, Canonical) | (Canonical, Plain)
    )
}

/// Lean `legal`: the five pairs the write gate admits.
fn legal(a: CanonizationStatus, b: CanonizationStatus) -> bool {
    eval_hop(a, b) || (a, b) == (Plain, Venerable)
}

/// Lean `Flags`: what the apply passes do to one concept.
#[derive(Clone, Copy, Debug)]
struct Flags {
    supersede: bool,
    stage1: bool,
    stage2: bool,
    stage3: bool,
    budget: bool,
}

/// Lean `cycle`: the five passes on one concept, from its status `s` when
/// the apply phase begins.
fn cycle(s: CanonizationStatus, f: Flags) -> CanonizationStatus {
    let s0 = if s == Canonical && f.supersede {
        Plain
    } else {
        s
    };
    let s1 = if s0 == Plain && !f.supersede && f.stage1 {
        Candidate
    } else {
        s0
    };
    let s2 = if s1 == Candidate && f.stage2 {
        Venerable
    } else {
        s1
    };
    let s3 = if s2 == Venerable && f.stage3 {
        Canonical
    } else {
        s2
    };
    if s3 == Canonical && f.budget {
        Plain
    } else {
        s3
    }
}

/// Per concept: its status when the plan was gathered (`g`), whether the
/// status changed before apply (and to what: `set_root_goal`, another
/// writer), and the plan and verdict flags. Lean `Flags`, plus what the
/// Rust verdicts carry: Stage 2 and 3 passes, the hold and deferral lists,
/// a Stage 3 blast, and a supersession verdict naming a source concept.
#[derive(Clone, Debug)]
struct Member {
    gathered: usize,
    moved: Option<usize>,
    planned: bool,
    passed: bool,
    held: bool,
    deferred: bool,
    blast: u64,
    supersede_by: Option<u64>,
}

fn member(n: u64) -> impl Strategy<Value = Member> {
    (
        0..4usize,
        proptest::option::weighted(0.3, 0..4usize),
        any::<bool>(),
        any::<bool>(),
        proptest::bool::weighted(0.2),
        proptest::bool::weighted(0.1),
        0u64..12,
        proptest::option::weighted(0.4, 0..n),
    )
        .prop_map(
            |(gathered, moved, planned, passed, held, deferred, blast, supersede_by)| Member {
                gathered,
                moved,
                planned,
                passed,
                held,
                deferred,
                blast,
                supersede_by,
            },
        )
}

/// `(members, max_canonical, order, stage3_skipped)`: the members, the
/// budget, the order the plan lists are built in, and whether the Stage 3
/// window was skipped (a speculative gather that no supersession freed).
type Case = (Vec<Member>, usize, Vec<usize>, bool);

fn case() -> impl Strategy<Value = Case> {
    (1u64..=10).prop_flat_map(|n| {
        let order: Vec<usize> = (0..n as usize).collect();
        (
            vec(member(n), n as usize),
            0..n as usize + 2,
            Just(order).prop_shuffle(),
            proptest::bool::weighted(0.15),
        )
    })
}

/// Supporting evidence every supersession source gets, so a verdict can
/// hold at apply (`supersession::still_holds`): `SUPPORT` concepts, each
/// from its own interaction (by an agent other than the contradiction's
/// declarer) an hour apart after the members' origin, each with a
/// `Dependency` edge into the source written with its interaction. That is
/// Stage 2's bar after the target's watermark: three distinct origins
/// (`MIN_DISTINCT`), all older than `min_age` at `now`, spread over two of
/// the session's three hours (coverage 0.67 >= `MIN_COVERAGE`), the shape
/// `src/test_util/supersession` builds. A target that is itself a source
/// has that evidence too, which moves its watermark past the verdict's, so
/// its verdict is refused at apply: both outcomes are drawn.
const SUPPORT: u64 = 3;

fn support_interaction(k: u64) -> NodeId {
    NodeId(uuid::Uuid::from_u64_pair(1, 10 + k))
}

fn support_concept(k: u64) -> NodeId {
    NodeId(uuid::Uuid::from_u64_pair(4, k))
}

fn support_at(k: u64) -> chrono::DateTime<chrono::Utc> {
    props::t0() + chrono::Duration::hours(k as i64 + 1)
}

/// Adds the supporting interactions and concepts to `g`.
fn add_support(g: &mut Graph) {
    let mut previous = Some(props::origin());
    for k in 0..SUPPORT {
        g.insert_interaction(Interaction {
            id: support_interaction(k),
            session_id: props::sid(),
            agent_id: AgentId::from("prop-support"),
            prompt_text: Some(format!("support {k}")),
            previous_id: previous,
            created_at: support_at(k),
            event_time: None,
        })
        .expect("support interaction");
        previous = Some(support_interaction(k));
        let mut c = props::concept(0, &format!("s{k}"), &format!("s{k}"), Plain);
        c.id = support_concept(k);
        c.origin_interaction = support_interaction(k);
        c.created_at = support_at(k);
        g.insert_concept(c, support_interaction(k))
            .expect("support concept");
    }
}

/// The member index of a concept id (`cid(i)`), `None` for any other id.
fn concept_index(id: NodeId) -> Option<usize> {
    let (hi, lo) = id.0.as_u64_pair();
    (hi == 2).then_some(lo as usize)
}

/// What one case committed, for the coverage guard.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    supersessions: usize,
    stage1: usize,
    stage2: usize,
    stage3: usize,
    budget: usize,
}

/// Builds one case's graph, plan and verdicts, runs the real `apply`, and
/// checks it against `Canon.lean` (see the property below).
fn check_cycle(
    members: &[Member],
    max_canonical: usize,
    order: &[usize],
    stage3_skipped: bool,
) -> Result<Tally, TestCaseError> {
    let n = members.len();
    let start = |m: &Member| STATUSES[m.moved.unwrap_or(m.gathered)];
    let mut g = props::graph_of(members.iter().enumerate().map(|(i, m)| {
        let text = format!("c{i}");
        props::concept(i as u64, &text, &text, start(m))
    }));
    add_support(&mut g);
    let gathered = |i: usize| STATUSES[members[i].gathered];
    let picked = |i: usize, s: CanonizationStatus| members[i].planned && gathered(i) == s;

    let mut plan = CyclePlan {
        session: props::sid(),
        supersede: Default::default(),
        hold: Default::default(),
        stage3_speculative: false,
        stage3_cursor_before: None,
        stage1: Vec::new(),
        stage2: Vec::new(),
        stage3: Vec::new(),
        demotion: Vec::new(),
    };
    let mut verdicts = Verdicts {
        stage3_skipped,
        ..Verdicts::default()
    };
    let now = props::t0() + chrono::Duration::days(1);
    let mut k = 0u64;
    let mut supported: HashSet<NodeId> = HashSet::new();
    let mut new_edge = |g: &mut Graph,
                        source: NodeId,
                        target: NodeId,
                        ty: EdgeType,
                        at: chrono::DateTime<chrono::Utc>| {
        k += 1;
        let id = NodeId(uuid::Uuid::from_u64_pair(3, k));
        g.upsert_edge(Edge {
            id,
            session_id: props::sid(),
            source,
            target,
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
            event_time: None,
        })
        .expect("edge");
        id
    };
    for &i in order {
        let (id, m) = (cid(i as u64), &members[i]);
        if picked(i, Plain) {
            plan.stage1.push(id);
        }
        if picked(i, Candidate) {
            plan.stage2.push(id);
            if m.passed {
                verdicts.stage2_pass.push(id);
            }
        }
        if picked(i, Venerable) {
            plan.stage3.push(Stage3Probe {
                node: id,
                last_demotion_time: None,
            });
            if m.passed {
                verdicts.stage3_pass.push((id, m.blast));
            }
            if m.held {
                verdicts.held.push(id);
            }
            if m.deferred {
                verdicts.deferred.push(id);
            }
        }
        if picked(i, Canonical) {
            plan.demotion.push((id, format!("c{i}")));
            verdicts
                .demotion_ranked
                .push((m.blast, id, format!("c{i}")));
        }
        if let Some(src) = m.supersede_by
            && gathered(i) == Canonical
            && src as usize != i
        {
            let source = cid(src);
            if supported.insert(source) {
                for s in 0..SUPPORT {
                    new_edge(
                        &mut g,
                        support_concept(s),
                        source,
                        EdgeType::Dependency,
                        support_at(s),
                    );
                }
            }
            let edge = new_edge(&mut g, source, id, EdgeType::Contradicts, props::t0());
            // The target's baseline (its origin's about-time) as the
            // watermark, as `decide` computes it for a target with no
            // evidence of its own.
            verdicts.supersede.push(supersession::Verdict {
                target: id,
                source,
                edge,
                watermark: props::t0(),
            });
        }
    }
    let params = EvalParams {
        max_canonical_nodes: max_canonical,
        supersede: true,
        ..EvalParams::default()
    };

    let before: Vec<CanonizationStatus> = (0..n).map(|i| start(&members[i])).collect();
    let canonical_before = before.iter().filter(|&&s| s == Canonical).count();
    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut outcome = EvalOutcome::default();
    let res = apply(
        &mut g,
        &tx,
        &plan,
        &verdicts,
        &params,
        now,
        &Default::default(),
        &mut outcome,
    );
    prop_assert!(res.is_ok(), "apply failed: {:?}", res);

    let mut moved = HashSet::new();
    for e in outcome.transitions() {
        prop_assert!(legal(e.from_status, e.to_status), "illegal event {:?}", e);
        prop_assert!(moved.insert(e.node_id), "two events for {}", e.node_id);
    }
    let after = |i: usize| concept_status(&g, cid(i as u64)).expect("concept");
    for (i, &s) in before.iter().enumerate() {
        let f = after(i);
        // cycle_one_hop (with evalHop_legal for the event above)
        prop_assert!(
            f == s || eval_hop(s, f),
            "concept {}: {:?} -> {:?}",
            i,
            s,
            f
        );
        // cycle_rank_le
        prop_assert!(rank(f) <= rank(s) + 1);
        // legal_demotion_unique
        if rank(f) < rank(s) {
            prop_assert!(s == Canonical && f == Plain, "demotion {:?} -> {:?}", s, f);
        }
        let id = cid(i as u64);
        // cycle with flags stage1: a Stage 1 member that is None when the
        // apply begins ends Candidate (no other pass can touch it: under
        // `Consistent` it is on no other list).
        if s == Plain && plan.stage1.contains(&id) {
            prop_assert_eq!(f, Candidate, "Stage 1 member {} not promoted", i);
        }
        // cycle with stage3 = false: a held or deferred probe never becomes
        // Canonical.
        if verdicts.held.contains(&id) || verdicts.deferred.contains(&id) {
            prop_assert!(
                !(f == Canonical && s != Canonical),
                "held/deferred {} promoted",
                i
            );
        }
    }
    // cycle_superseded_stays_none
    for e in &outcome.supersessions {
        prop_assert_eq!(
            concept_status(&g, e.node_id),
            Some(Plain),
            "superseded {} not None",
            e.node_id
        );
    }
    // Canon.cycle with every flag decided. Supersede: under this generator
    // a verdict holds exactly when its target starts apply Canonical and is
    // not itself a source, whose evidence would move its watermark past the
    // verdict's (see [`SUPPORT`]; no cooldowns are set). What committed must
    // be exactly that set. Swarm, the default policy, admits every hop whose
    // stage verdict passed. Stage 3's flag and the budget's depend on the
    // other concepts (the budget, the window order), so both passes are
    // replayed here over the statuses the earlier passes leave.
    let superseded: HashSet<NodeId> = verdicts
        .supersede
        .iter()
        .map(|v| v.target)
        .filter(|t| {
            concept_index(*t).is_some_and(|i| before[i] == Canonical) && !supported.contains(t)
        })
        .collect();
    let committed: HashSet<NodeId> = outcome.supersessions.iter().map(|e| e.node_id).collect();
    prop_assert_eq!(&committed, &superseded, "supersessions");
    for e in &outcome.supersessions {
        prop_assert!(
            verdicts.supersede.iter().any(|v| v.target == e.node_id),
            "{} superseded without a verdict",
            e.node_id
        );
    }
    let ids: Vec<NodeId> = (0..n).map(|i| cid(i as u64)).collect();
    let stage1 = |id: &NodeId| plan.stage1.contains(id);
    let stage2 = |id: &NodeId| plan.stage2.contains(id) && verdicts.stage2_pass.contains(id);
    let mut status: HashMap<NodeId, CanonizationStatus> = HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        let partial = Flags {
            supersede: superseded.contains(id),
            stage1: stage1(id),
            stage2: stage2(id),
            stage3: false,
            budget: false,
        };
        status.insert(*id, cycle(before[i], partial));
    }
    let canonical = |status: &HashMap<NodeId, CanonizationStatus>| {
        status.values().filter(|&&s| s == Canonical).count()
    };
    let mut remaining = max_canonical.saturating_sub(canonical(&status));
    let mut stage3_flag: HashSet<NodeId> = HashSet::new();
    // A skipped window was not evaluated: Stage 3 promotes nobody.
    let window: &[Stage3Probe] = if stage3_skipped { &[] } else { &plan.stage3 };
    prop_assert_eq!(
        &outcome.stage3_batch,
        &window.iter().map(|p| p.node).collect::<Vec<_>>(),
        "stage3_batch"
    );
    for probe in window {
        if remaining == 0 {
            break;
        }
        let id = probe.node;
        if status[&id] != Venerable
            || verdicts.held.contains(&id)
            || verdicts.deferred.contains(&id)
            || !verdicts.stage3_pass.iter().any(|&(p, _)| p == id)
        {
            continue;
        }
        stage3_flag.insert(id);
        status.insert(id, Canonical);
        remaining -= 1;
    }
    let mut overflow = canonical(&status).saturating_sub(max_canonical);
    let mut budget_flag: HashSet<NodeId> = HashSet::new();
    for &(_, id, _) in &verdicts.demotion_ranked {
        if overflow == 0 {
            break;
        }
        if status[&id] != Canonical {
            continue;
        }
        overflow -= 1;
        budget_flag.insert(id);
        status.insert(id, Plain);
    }
    for (i, id) in ids.iter().enumerate() {
        let flags = Flags {
            supersede: superseded.contains(id),
            stage1: stage1(id),
            stage2: stage2(id),
            stage3: stage3_flag.contains(id),
            budget: budget_flag.contains(id),
        };
        prop_assert_eq!(
            after(i),
            cycle(before[i], flags),
            "concept {} from {:?} with {:?}",
            i,
            before[i],
            flags
        );
    }
    // The same, stated on the events: every Stage 3 promotion had a passing
    // verdict, and the budget pass demoted min(overflow after Stage 3,
    // Canonical members of the ranking).
    for e in &outcome.promotions {
        if e.to_status == Canonical {
            prop_assert!(
                verdicts.stage3_pass.iter().any(|&(p, _)| p == e.node_id),
                "{} promoted to Canonical without a Stage 3 pass",
                e.node_id
            );
        }
    }
    prop_assert_eq!(
        outcome.demotions.len(),
        budget_flag.len(),
        "budget demotions"
    );

    // stage3_promotions_le and stage3_no_overflow: Stage 3 starts after the
    // supersessions (Stages 1 and 2 never touch Canonical), and promotes at
    // most what the budget has left.
    let stage3 = outcome
        .promotions
        .iter()
        .filter(|e| e.from_status == Venerable && e.to_status == Canonical)
        .count();
    let count = canonical_before - outcome.supersessions.len();
    prop_assert!(
        stage3 <= max_canonical.saturating_sub(count),
        "{} Stage 3 promotions with {} Canonical and a budget of {}",
        stage3,
        count,
        max_canonical
    );
    if count <= max_canonical {
        prop_assert!(
            count + stage3 <= max_canonical,
            "Stage 3 overflowed the budget"
        );
    }
    let canonical_after = (0..n).filter(|&i| after(i) == Canonical).count();
    prop_assert!(
        canonical_after <= max_canonical.max(canonical_before),
        "{} Canonical after, {} before, budget {}",
        canonical_after,
        canonical_before,
        max_canonical
    );

    let promoted = |from| {
        outcome
            .promotions
            .iter()
            .filter(|e| e.from_status == from)
            .count()
    };
    Ok(Tally {
        supersessions: outcome.supersessions.len(),
        stage1: promoted(Plain),
        stage2: promoted(Candidate),
        stage3,
        budget: outcome.demotions.len(),
    })
}

proptest! {
    #![proptest_config(props::config())]

    /// `Canon.cycle_one_hop`, `Canon.cycle_rank_le`, `Canon.evalHop_legal`,
    /// `Canon.legal_demotion_unique`, `Canon.cycle_superseded_stays_none`,
    /// `Canon.stage3_promotions_le` and `Canon.stage3_no_overflow`, and
    /// `Canon.cycle` itself where a test can decide its flags
    /// (`formal/LamboProofs/Canon.lean`), over random plans and verdicts run
    /// through the real `apply`:
    ///
    /// * the plan lists are consistent with a gather-time status (Lean
    ///   `Consistent`): Stage 1 lists only concepts gathered `None`, Stage 2
    ///   only `Candidate`, Stage 3 only `Venerable`, the budget ranking and
    ///   supersession verdicts only `Canonical`;
    /// * a concept's status may have moved between gather and apply, as the
    ///   theorem allows;
    /// * after `apply`, every concept is unchanged or has taken exactly one
    ///   eval-loop edge from its status at the start of apply; each committed
    ///   event is legal, and at most one per concept;
    /// * the only rank decrease is `Canonical -> None`;
    /// * every concept ends where Lean `cycle` puts it ([`cycle`]), with all
    ///   its flags decided from the plan, the verdicts and the budget; the
    ///   supersessions committed are exactly the verdicts whose target starts
    ///   apply Canonical and is not itself a source ([`SUPPORT`]); a skipped
    ///   Stage 3 window evaluates and promotes nobody. So a Stage 1 member
    ///   that is `None` at apply ends `Candidate`, a Stage 2 member promotes
    ///   iff it passed, a held, deferred or unpassed Stage 3 probe never
    ///   becomes Canonical, the budget pass demotes exactly the overflow it
    ///   can, and a superseded concept ends `None`. (Stage 1's own
    ///   `superseded` guard is unreachable here: under `Consistent` a Stage 1
    ///   member is never a supersession target. Lean
    ///   `cycle_superseded_stays_none` and the unit test
    ///   `a_superseded_concept_is_not_re_promoted_in_the_same_cycle` cover it.)
    /// * Stage 3 promotes at most the budget left after supersession, never
    ///   overflows a budget it started within, and the Canonical count ends
    ///   at most `max(max_canonical, count before)`.
    ///
    /// Supersession sources carry enough supporting evidence for
    /// `still_holds` to pass, so verdicts commit as well as get refused
    /// ([`SUPPORT`]); `every_kind_of_hop_commits` checks that they do.
    #[test]
    fn one_cycle_moves_each_concept_at_most_one_hop(
        (members, max_canonical, order, stage3_skipped) in case()
    ) {
        check_cycle(&members, max_canonical, &order, stage3_skipped)?;
    }
}

/// Coverage guard for the property above: the same strategy, run through a
/// deterministic `TestRunner`, commits supersessions, promotions at each
/// stage and budget demotions, so none of its checks is vacuous.
#[test]
fn every_kind_of_hop_commits() {
    // Pinned: the guard asks whether the strategy reaches every kind of hop
    // in 256 cases, which a small PROPTEST_CASES would make it fail.
    let config = Config {
        cases: 256,
        ..props::config()
    };
    let mut runner = TestRunner::new_with_rng(
        config.clone(),
        TestRng::deterministic_rng(config.rng_algorithm),
    );
    let total = RefCell::new(Tally::default());
    runner
        .run(
            &case(),
            |(members, max_canonical, order, stage3_skipped)| {
                let t = check_cycle(&members, max_canonical, &order, stage3_skipped)?;
                let mut total = total.borrow_mut();
                total.supersessions += t.supersessions;
                total.stage1 += t.stage1;
                total.stage2 += t.stage2;
                total.stage3 += t.stage3;
                total.budget += t.budget;
                Ok(())
            },
        )
        .expect("the property holds");
    let total = total.into_inner();
    assert!(
        total.supersessions > 0
            && total.stage1 > 0
            && total.stage2 > 0
            && total.stage3 > 0
            && total.budget > 0,
        "a kind of hop never committed: {total:?}"
    );
}
