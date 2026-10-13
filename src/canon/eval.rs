//! Evaluation cycle — promotions, budget demotion, audit (T6.4, spec §10).
//!
//! One hop per cycle (documented): a node may take **at most one** legal
//! edge of the state machine per tick. The eval loop never uses
//! `None → Venerable` (that edge is reserved for `set_root_goal`). The
//! hops this cycle will emit, in commit order, are:
//!
//! 0. Supersession (#20, only with [`EvalParams::supersede`]):
//!    `Canonical → None` for a concept whose contradicting concept has newer
//!    dependent evidence that clears Stage 2 (see
//!    [`crate::canon::supersession`](super::supersession)). It commits
//!    **first**, so a slot it frees is spent by this cycle's Stage 3, and a
//!    concept it demotes takes no other hop this cycle.
//! 1. Stage 1: `None → Candidate` for the first [`EvalParams::batch_size`]
//!    still-None nodes the policy admits, in the policy's order (Swarm:
//!    [`crate::canon::stage1_candidates`], evidence-led and composite-ordered;
//!    Solo: score band, score-ordered).
//! 2. Stage 2: `Candidate → Venerable` for nodes that were already
//!    Candidate *before* this cycle's Stage 1 hop (a node that just
//!    became Candidate is not re-checked for Venerable in the same tick).
//!    The hop needs the Stage 2 verdict under **both** policies (#36 PR 3);
//!    Solo also needs its band.
//! 3. Stage 3: `Venerable → Canonical` for a **round-robin** window of
//!    at most [`EvalParams::batch_size`] Venerable nodes, **score-
//!    descending** (canonical key ascending, then NodeId ascending —
//!    [`tie_break_by_key`], issue #2) within the window. A member's Stage 3
//!    verdict requires it to pass Stage 2 again (#36 upgrade guard), blast
//!    radius above five and the re-promotion cooldown clear, under both
//!    policies; Solo also needs its band. The cursor lives on [`Evaluator`]
//!    so the next cycle continues around the ring.
//! 4. Budget: if Canonical count exceeds `max_canonical_nodes`, demote
//!    `Canonical → None` lowest [`GraphStore::blast_radius`] first
//!    (canonical key ascending, then NodeId ascending —
//!    [`tie_break_by_key`], issue #2) until the count is within budget.
//!
//! [`EvalOutcome::transitions`] and phase 4's audit follow that commit
//! order: supersessions, then promotions, then budget demotions.
//!
//! One hop per cycle is **structural**, not bookkeeping: all three stage
//! windows are read from the same pre-cycle graph state, where the `None` /
//! `Candidate` / `Venerable` sets are disjoint by definition. A node that
//! becomes Candidate in this cycle's Stage 1 was not in the Stage 2 window.
//!
//! ## Shape — gather → verdicts → apply → record (spec §6.4)
//!
//! The production owner holds the graph in `Arc<RwLock<Graph>>` and
//! `parking_lot` guards are `!Send`, so no guard may be alive across an
//! `.await` — the same rule the daemon loop is built around
//! (`src/daemon/mod.rs`, "Lock discipline"). A cycle that took `&mut Graph`
//! and awaited store calls underneath it therefore had no legal caller at
//! all. The cycle is instead four phases:
//!
//! 1. [`Evaluator::gather`] — **synchronous, read guard.** Reads the three
//!    stage windows, the Stage-3 cooldown inputs and the budget probe out of
//!    the graph. No I/O.
//! 2. [`verdicts`] — **async, no lock.** One `interaction_span_with_window`
//!    per Stage-2 window member; one `blast_radius` per Stage-3 window member,
//!    preceded by an `interaction_span_with_window` and skipped when that
//!    fails; and one `blast_radius` per budget probe. Both policies ask the
//!    same queries (#36 PR 3: Solo consumes the verdicts). A store without
//!    the window-aware span gives a node no Stage 2 evidence and one warning
//!    per cycle, not a failed cycle. Touches no graph.
//! 3. [`apply`] — **synchronous, write guard.** Re-checks each node's
//!    current status, applies the transitions, emits `DaemonEvent::Canonized`.
//! 4. [`record`] — **async, no lock.** `store.record_canonization` per hop.
//!
//! [`Evaluator::eval_cycle`] composes the four over an `&RwLock<Graph>`;
//! [`crate::canon::CanonizationTask`] drives it every
//! `canonization_eval_interval` (spec §10's "every 60s").
//!
//! ## Commit point
//!
//! The **graph apply** is the commit point, and every hop goes through
//! [`commit_transition`]:
//!
//! 1. [`Graph::apply_canonization_transition`] — RAM + in-graph audit + the
//!    write-behind mutation log.
//! 2. [`events::emit_canonized`] — `DaemonEvent::Canonized`.
//!
//! [`GraphStore::record_canonization`] follows in phase 4. Emission is at the
//! commit point rather than after that store round-trip because the apply is
//! what makes the transition real: it is in RAM, in the audit, and in the
//! write-behind log, so the store learns of it on the next flush regardless.
//! Ordering the emit behind the immediate durable write meant a single
//! `record_canonization` failure lost the `Canonized` event **forever** (the
//! flush replay re-records the row but publishes nothing) — by this phase's
//! own standard, a demo bug.
//!
//! For the same reason a failed cycle returns [`EvalError`], which carries
//! the partial [`EvalOutcome`]: the hops committed before the failure are
//! real and the caller must not have them silently dropped on the floor.
//! A failed *apply* still does not emit — a fabricated transition is worse
//! than a missing one.
//!
//! ## Store faults and Stage 1 (R2-4)
//!
//! Phase 2 issues **every** store query before phase 3 commits **anything**,
//! so a cycle is never half-applied against a half-answered store. The cost
//! is that one failing query would fail the whole cycle — including Stage 1,
//! which asks the store nothing and could always have landed. Under a store
//! outage that stopped progression dead at the first stage.
//!
//! So a phase-2 error is not fatal to Stage 1: the cycle applies the Stage-1
//! plan alone, through the same commit point, and returns it inside the
//! [`EvalError`]'s outcome. Stages 2 and 3 and the budget probe are dropped
//! whole — they have no verdicts, and a stage never runs on a guess. Phase 4
//! is skipped for those hops (the store is the thing that just failed); the
//! write-behind log carries them on the next flush, deduped on event id, the
//! same guarantee a failed `record_canonization` leans on.
//!
//! `now` is injected — the cycle has no wall clock, and neither do the store
//! queries it issues (see [`crate::store::GraphStore::blast_radius`]).

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use crate::canon::stage3;
use crate::canon::supersession;
use crate::canon::{stage2_passes_with_window, PromotionPolicy};
use crate::daemon::events::{self, EventSender};
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    tie_break_by_key, CanonizationEvent, CanonizationStatus, LamboError, Node, NodeId, ScoreTable,
    SessionId, StoreError,
};

/// Round-robin cursors plus the one-cycle write path.
#[derive(Clone, Debug, Default)]
pub struct Evaluator {
    /// Last Stage-2 Candidate evaluated; the next cycle resumes after it.
    stage2_cursor: Option<NodeId>,
    /// Last Stage-3 Venerable evaluated; the next cycle resumes after it.
    stage3_cursor: Option<NodeId>,
    /// Last supersession target probed (#20); the next cycle resumes after it.
    supersession_cursor: Option<NodeId>,
}

/// Knobs for one [`eval_cycle`]. Defaults match [`crate::Config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalParams {
    /// **Inert since #36 PR 3.** Swarm's Stage 1 used to need this many
    /// non-Canonical peers before its P90 cut opened; evidence-led admission
    /// has no peer minimum and no policy reads the field. Kept so existing
    /// callers compile; [`crate::Config::warnings`] names a non-default value.
    #[deprecated(
        since = "0.6.0",
        note = "inert since #36 PR 3: Stage 1 admits on evidence with no peer minimum"
    )]
    pub min_peer_count: usize,
    /// Age floor for Stage 1's dependent interactions and Stage 2's
    /// `interaction_span_with_window`.
    pub min_age: Duration,
    /// Denominator cap forwarded to Stage 2; no evidence expiry.
    pub coverage_window: Duration,
    /// Forwarded to Stage 3 / budget (`blast_radius` age floor).
    pub min_edge_age: Duration,
    pub cooldown: Duration,
    /// Nodes considered per stage per cycle (spec default 50).
    ///
    /// Spec §10 names this bound for the Stage-3 Venerable ring. It also caps
    /// Stage 1's hops and Stage 2's window (F13): every member of either is a
    /// per-node store round-trip or a durable write, so an uncapped stage
    /// issues N sequential queries per tick against Cockroach forever. The
    /// spec fixes no *lower* bound on throughput, so capping is compatible;
    /// what it costs is latency, which the cursors bound fairly.
    pub batch_size: usize,
    pub max_canonical_nodes: usize,
    /// Which Stage-1 promotion policy this cycle runs (C1).
    ///
    /// A *selector*, not a threshold: it chooses which predicate reads the
    /// knobs above, and duplicates none of them. Default
    /// [`PromotionPolicy::Swarm`] — the policy the pipeline has always run.
    pub promotion_policy: PromotionPolicy,
    /// Evidence-driven supersession (#20), from
    /// [`crate::Config::canonization_supersede`]. Default `false`: the cycle
    /// is exactly the pre-#20 one.
    pub supersede: bool,
}

/// What one cycle wrote. `supersessions`, then `promotions`, then
/// `demotions` is the commit order ([`Self::transitions`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvalOutcome {
    /// `Canonical → None` demotions by supersession (#20), committed first;
    /// each carries `reason = Superseded { by }`. Empty unless
    /// [`EvalParams::supersede`].
    pub supersessions: Vec<CanonizationEvent>,
    pub promotions: Vec<CanonizationEvent>,
    /// Budget demotions, committed last.
    pub demotions: Vec<CanonizationEvent>,
    /// The Stage 3 window this cycle ran through the predicate, in the order
    /// it was evaluated (score-descending).
    ///
    /// F10: this is the **evaluated** window, not a candidate list — every
    /// node here was run through the Stage-3 predicate, including the ones
    /// that failed it and the ones that passed but found the budget spent.
    /// A cycle with no remaining budget evaluates nothing, so it lists
    /// nothing and does not step the cursor either (R2-2).
    pub stage3_batch: Vec<NodeId>,
}

/// A cycle that failed partway, carrying what it had already committed.
///
/// The old `?`-per-hop shape discarded the whole [`EvalOutcome`] on the first
/// store error — including hops already applied to the graph, emitted, and
/// durably recorded earlier in the same cycle. The caller needs both halves:
/// the error to log and back off on, the outcome to account for.
#[derive(Debug)]
pub struct EvalError {
    /// Every hop this cycle committed to the graph before it failed.
    pub outcome: EvalOutcome,
    /// What went wrong.
    pub source: LamboError,
}

impl EvalError {
    fn new(outcome: EvalOutcome, source: impl Into<LamboError>) -> Self {
        Self {
            outcome,
            source: source.into(),
        }
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "canonization cycle failed after {} committed transition(s): {}",
            self.outcome.transitions().count(),
            self.source
        )
    }
}

impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<EvalError> for LamboError {
    fn from(err: EvalError) -> Self {
        err.source
    }
}

impl EvalParams {
    // Copies the deprecated inert field so library callers that still read
    // it see the configured value.
    #[allow(deprecated)]
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            min_peer_count: config.canonization_min_peer_count,
            min_age: config.canonization_edge_min_age,
            coverage_window: config.canonization_coverage_window,
            min_edge_age: config.canonization_edge_min_age,
            cooldown: config.canonization_repromotion_cooldown,
            batch_size: config.canonization_eval_batch_size,
            max_canonical_nodes: config.max_canonical_nodes,
            promotion_policy: config.promotion_policy,
            supersede: config.canonization_supersede,
        }
    }
}

impl Default for EvalParams {
    fn default() -> Self {
        Self::from_config(&crate::Config::default())
    }
}

impl EvalOutcome {
    /// Every hop this cycle committed, in commit order: supersessions,
    /// promotions, budget demotions.
    pub fn transitions(&self) -> impl Iterator<Item = &CanonizationEvent> {
        self.supersessions
            .iter()
            .chain(self.promotions.iter())
            .chain(self.demotions.iter())
    }

    /// Whether this cycle committed nothing (the steady state).
    pub fn is_empty(&self) -> bool {
        self.supersessions.is_empty() && self.promotions.is_empty() && self.demotions.is_empty()
    }
}

/// Everything one cycle reads from the graph, captured under a single read
/// guard so the verdict phase can run with no lock held.
#[derive(Clone, Debug, PartialEq)]
struct CyclePlan {
    session: SessionId,
    /// Supersession (#20): bounded `(target, source)` probes, empty unless
    /// [`EvalParams::supersede`].
    supersede: Vec<supersession::Probe>,
    /// The hold (#20): the same probes for Stage 3's Venerable window.
    hold: Vec<supersession::Probe>,
    /// Stage 1: still-`None` concepts clearing the Candidate predicate.
    stage1: Vec<NodeId>,
    /// Stage 2: still-`Candidate` window off the identity cursor.
    stage2: Vec<NodeId>,
    /// Stage 3: Venerable window off the identity cursor, score-descending.
    /// Empty when the Canonical budget is already full; otherwise the whole
    /// window — the budget cut happens in `apply`, on the nodes that passed
    /// (R2-2).
    stage3: Vec<Stage3Probe>,
    /// Budget: Canonical ids to rank for demotion, with each concept's
    /// canonical key (the issue-2 tie-break in `verdicts`, which holds no
    /// graph). Empty unless the session is **already** over budget: Stage 3
    /// is capped at the remaining budget, so a cycle can never create the
    /// overflow it then demotes (the phase-R2 P2 / original P1-1 property).
    demotion: Vec<(NodeId, String)>,
}

/// One Stage-3 window member plus the cooldown input read from its concept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stage3Probe {
    node: NodeId,
    last_demotion_time: Option<DateTime<Utc>>,
}

/// The store's answers for one [`CyclePlan`], computed with no lock held.
#[derive(Clone, Debug, Default, PartialEq)]
struct Verdicts {
    /// Supersession (#20): one winning source per target.
    supersede: Vec<supersession::Verdict>,
    /// Stage-3 window members a contradicting concept would supersede at
    /// once (#20's hold): not promoted this cycle.
    held: Vec<NodeId>,
    /// Stage-2 window members that cleared the span predicate.
    stage2_pass: Vec<NodeId>,
    /// `(node, measured blast)` for Stage-3 admissions, in evaluation order.
    /// The measurement is the one that admitted the node — it is what the
    /// audit row is stamped with (F9), never a second query.
    stage3_pass: Vec<(NodeId, u64)>,
    /// `(blast, node, key)` for the budget ranking, blast-ascending, then
    /// canonical key ascending, then NodeId-ascending (spec §10: lowest blast
    /// radius demoted first; the issue-2 tie-break keeps equal-blast order
    /// stable across runs).
    demotion_ranked: Vec<(u64, NodeId, String)>,
}

impl Evaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last Stage-3 Venerable this evaluator evaluated (tests; the next
    /// cycle resumes at the first ring element strictly greater than it).
    pub fn stage3_cursor(&self) -> Option<NodeId> {
        self.stage3_cursor
    }

    /// The last Stage-2 Candidate this evaluator evaluated (tests).
    pub fn stage2_cursor(&self) -> Option<NodeId> {
        self.stage2_cursor
    }

    /// One eval cycle. See the module docs for hop order and phase order.
    ///
    /// Takes the lock itself, in three short scopes, so the `!Send` guards
    /// structurally cannot span the store I/O — the future this returns is
    /// `Send` and can be `tokio::spawn`ed.
    #[allow(clippy::too_many_arguments)]
    // one cycle's full input; mirrors eval_cycle()
    // EvalError carries the partial EvalOutcome by value (a public field);
    // #20's `supersessions` vector took it past clippy's size bound. The
    // error path is once per failed 60 s cycle, so boxing buys nothing and
    // would break the field's type.
    #[allow(clippy::result_large_err)]
    pub async fn eval_cycle(
        &mut self,
        graph: &RwLock<Graph>,
        store: &dyn GraphStore,
        scores: &ScoreTable,
        events: &EventSender,
        params: &EvalParams,
        now: DateTime<Utc>,
        token: Option<u64>,
    ) -> Result<EvalOutcome, EvalError> {
        // 1. Gather — read guard, released before the first await.
        let plan = {
            let g = graph.read();
            self.gather(&g, scores, params, now)
        };

        // 2. Verdicts — store I/O, no lock held.
        let verdicts = match verdicts(store, &plan, params, now).await {
            Ok(verdicts) => verdicts,
            // R2-4: Stage 1 asked the store nothing, so a store fault is no
            // reason to hold its hops back. Every Stage-2/Stage-3 query runs
            // before anything commits (the atomicity that makes a half-applied
            // cycle impossible), which also means one failing query used to
            // halt the whole cycle — including a stage that needs no verdict
            // at all. Under a store outage that stalled progression entirely;
            // at 06fcc00 the Stage-1 hops landed before Stage 2's first call.
            //
            // So: apply the Stage-1 plan alone — same commit point, same
            // emit, and the partial outcome names what landed. Stages 2/3 and
            // the budget probe are dropped whole, never half-applied.
            // Phase 4 is skipped: the store just failed, and the write-behind
            // log carries these hops to it on the next flush (deduped on
            // event id) exactly as it does for a failed `record`.
            Err(err) => {
                let stage1_only = CyclePlan {
                    session: plan.session.clone(),
                    supersede: Vec::new(),
                    hold: Vec::new(),
                    stage1: plan.stage1.clone(),
                    stage2: Vec::new(),
                    stage3: Vec::new(),
                    demotion: Vec::new(),
                };
                let mut outcome = EvalOutcome::default();
                let applied = {
                    let mut g = graph.write();
                    apply(
                        &mut g,
                        events,
                        &stage1_only,
                        &Verdicts::default(),
                        params,
                        now,
                        &mut outcome,
                    )
                };
                // A failed apply is the more specific fault; either way the
                // caller gets what committed.
                if let Err(apply_err) = applied {
                    return Err(EvalError::new(outcome, apply_err));
                }
                return Err(EvalError::new(outcome, err));
            }
        };

        // 3. Apply — write guard, released before the next await. Commit point.
        let mut outcome = EvalOutcome::default();
        let applied = {
            let mut g = graph.write();
            apply(&mut g, events, &plan, &verdicts, params, now, &mut outcome)
        };
        if let Err(err) = applied {
            return Err(EvalError::new(outcome, err));
        }

        if let Err(err) = record(store, &outcome, token).await {
            return Err(EvalError::new(outcome, err));
        }
        Ok(outcome)
    }

    /// Phase 1 — read the cycle's inputs out of the graph and advance the
    /// cursors. Synchronous: the caller holds the read guard.
    fn gather(
        &mut self,
        graph: &Graph,
        scores: &ScoreTable,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> CyclePlan {
        let session = graph.session_id().clone();

        // Supersession (#20) — bounded probes off its own target ring. Read
        // first: whether any exist decides whether Stage 3 is gathered at a
        // full budget below.
        let supersede = supersession::gather(graph, params, now, &mut self.supersession_cursor);

        // The scores are the daemon's and only order Stage 1's queue and the
        // Stage 3 window (#36 PR 3: they no longer select anyone). A table
        // older than the graph orders concepts born since the last rescore
        // at 0.0. The daemon's rescore is epoch-gated and runs on its own (1s)
        // tick while this cycle runs on a 60s one, so a brief lag is expected
        // rather than a fault: report it and proceed.
        if scores.epoch != graph.epoch() {
            tracing::debug!(
                target: "lambo::canon",
                scores_epoch = scores.epoch,
                graph_epoch = graph.epoch(),
                "canonization cycle running on a score table older than the graph"
            );
        }

        // Stage 1 — still-None candidates in the policy's order (Swarm: the
        // composite, highest first; Solo: its resistant score), capped at the
        // batch. No cursor: the set drains (a promoted node leaves it, and
        // the next cycle takes the next members), unlike Stage 2, whose
        // members can fail their evidence gate cycle after cycle.
        //
        // C1: the predicate is chosen by `promotion_policy` rather than
        // welded in. `now` and `params.min_age` age the dependence evidence
        // both policies read (#36 PR 3).
        let stage1: Vec<NodeId> = params
            .promotion_policy
            .scorer()
            .candidates(graph, scores, params, now)
            .into_iter()
            .filter(|&id| concept_status(graph, id) == Some(CanonizationStatus::None))
            .take(params.batch_size)
            .collect();

        // Stage 2 — one `interaction_span_with_window` round-trip per member, so the
        // window is capped and walks the identity cursor (F13).
        //
        // #36 PR 3 review L1: the ring holds only members the policy could
        // admit (`admits_hop` for the hop, asked before the verdict). Swarm
        // admits every member, so its ring is unchanged; Solo drops the
        // Candidates whose band cannot reach Venerable, which would cost a
        // store probe every pass and could never promote.
        let scorer = params.promotion_policy.scorer();
        let mut candidates = ids_with_status(graph, CanonizationStatus::Candidate);
        candidates
            .retain(|&id| scorer.admits_hop(graph, id, CanonizationStatus::Venerable, params, now));
        let stage2 = ring_window(&candidates, self.stage2_cursor, params.batch_size);
        if let Some(&last) = stage2.last() {
            self.stage2_cursor = Some(last);
        }

        // Stage 3 — the Venerable ring. The Canonical budget gates whether
        // this stage runs at all, and nothing finer:
        //
        // * `remaining == 0` — take **nothing**. Not one node can promote, so
        //   evaluating is pure cost and rotating the ring is a lie. The old
        //   shape took the window and then broke out of the promotion loop, so
        //   the cursor advanced over nodes it never evaluated and
        //   `stage3_batch` claimed them anyway (F1's related note, F10).
        // * `remaining > 0` — take the whole ring window and evaluate all of
        //   it. **R2-2**: truncating to `remaining` here starved the ring.
        //   The window is ranked score-descending, so when the ring fits in
        //   `batch_size` (the common case) the same top-`remaining` members
        //   were the only ones ever evaluated — a top-scoring Venerable that
        //   cannot pass (blast <= 5, or cooling) held the slot forever and the
        //   Canonical budget never filled. The ranking's job is to decide who
        //   wins the last slot among the nodes that **pass**, which is not
        //   knowable until the verdicts are in; `apply` does that cut, under
        //   the write guard, against a freshly recomputed budget.
        // * #20: with supersession probes pending, a full budget may free
        //   slots this cycle (supersession commits first), so the window is
        //   gathered and evaluated anyway. Otherwise a full budget at gather
        //   would starve the Venerables of the very slot supersession frees.
        let remaining = params
            .max_canonical_nodes
            .saturating_sub(canonical_count(graph));
        let mut venerable = ids_with_status(graph, CanonizationStatus::Venerable);
        // Same filter as Stage 2's ring (review L1).
        venerable
            .retain(|&id| scorer.admits_hop(graph, id, CanonizationStatus::Canonical, params, now));
        let mut window = if remaining == 0 && supersede.is_empty() {
            Vec::new()
        } else {
            ring_window(&venerable, self.stage3_cursor, params.batch_size)
        };
        if let Some(&last) = window.last() {
            self.stage3_cursor = Some(last);
        }
        // Score-descending within the window (spec §10), then the issue-2
        // tie-break (canonical key asc, NodeId asc): the evaluation order,
        // and therefore the order `apply` spends the budget in. Equal-score
        // Venerables must hold one order across runs, so the id alone cannot
        // decide. The cursor is anchored in RING order, taken above.
        let key = |id: NodeId| match graph.node(id) {
            Some(crate::types::Node::Concept(c)) => Some(c.canonical_key.as_str()),
            _ => None,
        };
        let score_of = score_map(scores);
        window.sort_by(|a, b| {
            score_lookup(&score_of, *b)
                .total_cmp(&score_lookup(&score_of, *a))
                .then_with(|| tie_break_by_key(key(*a), a, key(*b), b))
        });
        // #20's hold: the same probes for the window, so a Venerable that
        // would be superseded at once is not promoted.
        let hold = supersession::gather_holds(graph, &window, params, now);
        let stage3: Vec<Stage3Probe> = window
            .into_iter()
            .map(|node| Stage3Probe {
                node,
                last_demotion_time: stage3::last_demotion_time(graph, node),
            })
            .collect();

        // Budget — probe only when the session is already over the ceiling.
        // Keys ride along because `verdicts` (which ranks the demotion) holds
        // no graph; the ids alone would order equal-blast ties per-run
        // arbitrarily (issue #2).
        let canonicals = ids_with_status(graph, CanonizationStatus::Canonical);
        let demotion = if canonicals.len() > params.max_canonical_nodes {
            canonicals
                .into_iter()
                .map(|id| {
                    let key = match graph.node(id) {
                        Some(crate::types::Node::Concept(c)) => c.canonical_key.clone(),
                        _ => String::new(),
                    };
                    (id, key)
                })
                .collect()
        } else {
            Vec::new()
        };

        CyclePlan {
            session,
            supersede,
            hold,
            stage1,
            stage2,
            stage3,
            demotion,
        }
    }
}

/// The next `size` ids of `ring` starting at the first element **strictly
/// greater** than `cursor`, wrapping. `ring` must be NodeId-ascending.
///
/// The cursor is an **identity**, not an index. A positional cursor into a
/// vector rebuilt every cycle skids whenever the ring changes shape:
/// promoting the window's members removes them, every later element shifts
/// left, and the next window starts *past* the longest-waiting nodes. With a
/// steady Stage-2 inflow straddling them in sort order the skid repeats and
/// those nodes are never evaluated again — anti-starvation lost, silently.
/// Anchoring on the last id evaluated is churn-immune by construction and
/// costs one binary search.
pub(super) fn ring_window(ring: &[NodeId], cursor: Option<NodeId>, size: usize) -> Vec<NodeId> {
    if ring.is_empty() || size == 0 {
        return Vec::new();
    }
    let n = ring.len();
    // `partition_point` is the first index whose id is strictly greater than
    // the cursor; `% n` wraps when the cursor is at or past the ring's end
    // (including the case where the cursor's node has left the ring entirely).
    let start = match cursor {
        Some(last) => ring.partition_point(|id| id.0 <= last.0) % n,
        None => 0,
    };
    let take = size.min(n);
    (0..take).map(|i| ring[(start + i) % n]).collect()
}

/// Phase 2 — the store's verdicts for `plan`. No lock is held here.
async fn verdicts(
    store: &dyn GraphStore,
    plan: &CyclePlan,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<Verdicts, StoreError> {
    // #20: supersession and the hold in one probe pass, so a store without
    // the accessors gives no verdicts and one warning; any other error fails
    // the cycle like every query here. The two target sets are disjoint
    // (Canonical, Venerable).
    let probes: Vec<supersession::Probe> =
        plan.supersede.iter().chain(&plan.hold).cloned().collect();
    let (held, supersede): (Vec<_>, Vec<_>) =
        supersession::verdicts(store, &plan.session, &probes, params, now)
            .await?
            .into_iter()
            .partition(|v| plan.hold.iter().any(|p| p.target == v.target));
    let mut out = Verdicts {
        supersede,
        held: supersession::held(&held),
        ..Verdicts::default()
    };
    // #36 PR 3: both policies consume the Stage 2 verdict for the
    // Candidate → Venerable hop and re-check it before Stage 3 (PR 1's
    // upgrade guard, PR 2 review M1 had Solo skip it because Solo then
    // ignored the verdict). The match is exhaustive so a new policy has to
    // decide.
    let (query_stage2, recheck_stage2) = match params.promotion_policy {
        PromotionPolicy::Swarm | PromotionPolicy::Solo => (true, true),
    };
    // The capability a store lacks, if its window-aware span refused this
    // cycle. Collected so one warning names it, not one per node.
    let mut missing: Option<String> = None;
    if query_stage2 {
        for &id in &plan.stage2 {
            if stage2_verdict(store, &plan.session, id, params, now, &mut missing).await? {
                out.stage2_pass.push(id);
            }
        }
    }
    for probe in &plan.stage3 {
        // #36 upgrade guard: a Venerable is re-checked against Stage 2 before
        // its Stage 3 probe. Stage 2 is not a one-time admission. A node
        // admitted under an older definition of support (before #36, inbound
        // `Causal` edges counted; under Solo before PR 3, the band alone), or
        // whose support has since thinned, must still hold the evidence the
        // ladder rests on before it can become Canonical. The Stage 3 verdict
        // is "Stage 2 still holds, blast > 5 and the cooldown is clear", and
        // the span query runs first so a failing node costs no blast probe.
        if recheck_stage2
            && !stage2_verdict(store, &plan.session, probe.node, params, now, &mut missing).await?
        {
            continue;
        }
        if let Some(blast) = stage3::stage3_passes(
            store,
            &plan.session,
            probe.node,
            probe.last_demotion_time,
            params.min_edge_age,
            params.cooldown,
            now,
        )
        .await?
        {
            out.stage3_pass.push((probe.node, blast));
        }
    }
    for (id, key) in &plan.demotion {
        let blast = store
            .blast_radius(&plan.session, *id, params.min_edge_age, now)
            .await?;
        out.demotion_ranked.push((blast, *id, key.clone()));
    }
    out.demotion_ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| tie_break_by_key(Some(&a.2), &a.1, Some(&b.2), &b.1))
    });
    if let Some(capability) = missing {
        tracing::warn!(
            target: "lambo::canon",
            capability = %capability,
            "CanonizationStage2Unsupported: the store does not implement \
             interaction_span_with_window, so no concept passes Stage 2 or \
             Stage 3 this cycle; Stage 1 and the budget still ran"
        );
    }
    Ok(out)
}

/// One Stage 2 verdict, under either policy. A store that lacks the window-aware span
/// (`StoreError::Capability`, the trait default) gives this node **no
/// evidence** rather than failing the cycle (#36 PR 2 review M1): Stage 2
/// fails closed for the node, and Stage 1, Stage 3's other probes and the
/// budget still run. The capability is recorded once for the cycle's
/// warning. Any other store error still fails the cycle.
async fn stage2_verdict(
    store: &dyn GraphStore,
    session: &crate::types::SessionId,
    node: crate::types::NodeId,
    params: &EvalParams,
    now: DateTime<Utc>,
    missing: &mut Option<String>,
) -> Result<bool, StoreError> {
    match stage2_passes_with_window(
        store,
        session,
        node,
        params.min_age,
        now,
        params.coverage_window,
    )
    .await
    {
        Ok(pass) => Ok(pass),
        Err(StoreError::Capability(what)) => {
            missing.get_or_insert(what);
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// Phase 3 — apply the verdicts. Synchronous: the caller holds the write
/// guard, and this is the cycle's commit point.
///
/// Every hop re-checks the node's **current** status first: the graph was
/// unlocked while the verdicts were computed, so another writer may have
/// moved it. `outcome` is filled in as hops commit, so a mid-phase failure
/// still hands the caller everything that did.
fn apply(
    graph: &mut Graph,
    events: &EventSender,
    plan: &CyclePlan,
    verdicts: &Verdicts,
    params: &EvalParams,
    now: DateTime<Utc>,
    outcome: &mut EvalOutcome,
) -> Result<(), LamboError> {
    outcome.stage3_batch = plan.stage3.iter().map(|p| p.node).collect();

    // Every later hop needs its stage verdict first (#36 PR 3: no policy can
    // admit a hop the evidence refuses), then the policy seam has the final
    // word: Swarm keeps the verdict as the whole decision, Solo also needs
    // its score band.
    let scorer = params.promotion_policy.scorer();

    // --- Supersession (#20): Canonical → None, committed first ---
    // Each verdict is re-checked against the graph as it is now (the store
    // was probed with no lock held), and a concept demoted here takes no
    // other hop this cycle. Re-checking in target order also settles
    // reciprocal contradictions: the first demotion stamps its concept's
    // cooldown, so that concept can no longer supersede the other.
    let mut superseded: Vec<NodeId> = Vec::new();
    if !verdicts.supersede.is_empty() {
        let timeline = crate::daemon::conflict::WriterTimeline::of(graph);
        for verdict in &verdicts.supersede {
            if !supersession::still_holds(graph, verdict, params, now, &timeline) {
                tracing::debug!(
                    target: "lambo::canon",
                    node = %verdict.target,
                    by = %verdict.source,
                    "supersession no longer holds at apply; skipped"
                );
                continue;
            }
            let event = supersession::event(graph, verdict, now);
            outcome
                .supersessions
                .push(commit_transition(graph, events, event)?);
            superseded.push(verdict.target);
        }
    }

    // --- Stage 1: None → Candidate (one hop; no skip to Venerable) ---
    for &id in &plan.stage1 {
        if concept_status(graph, id) != Some(CanonizationStatus::None) || superseded.contains(&id) {
            continue;
        }
        let event = promotion_event(
            graph,
            id,
            CanonizationStatus::None,
            CanonizationStatus::Candidate,
            None,
            now,
        );
        outcome
            .promotions
            .push(commit_transition(graph, events, event)?);
    }

    // --- Stage 2: Candidate → Venerable ---
    // The window is walked in ring order; a member promotes iff it passed
    // Stage 2 and its policy admits the hop.
    for &id in &plan.stage2 {
        if concept_status(graph, id) != Some(CanonizationStatus::Candidate) {
            continue;
        }
        if !verdicts.stage2_pass.contains(&id) {
            continue;
        }
        if !scorer.admits_hop(graph, id, CanonizationStatus::Venerable, params, now) {
            continue;
        }
        let event = promotion_event(
            graph,
            id,
            CanonizationStatus::Candidate,
            CanonizationStatus::Venerable,
            None,
            now,
        );
        outcome
            .promotions
            .push(commit_transition(graph, events, event)?);
    }

    // --- Stage 3: Venerable → Canonical, capped at the remaining budget ---
    // This is the only budget cut (R2-2): the verdicts cover the whole ring
    // window, and the passing nodes are spent against the budget in
    // score-descending order. Recomputed here, under the write guard, because
    // the graph was unlocked while the verdicts ran.
    //
    // A node needs a passing Stage 3 verdict (which already includes the
    // Stage 2 re-check and the cooldown) and its policy's admission. It
    // keeps the measurement that admitted it: the audit row is stamped with
    // exactly that value (F9). Before #36 PR 3 Solo could promote here on its
    // band with no verdict, gated only by the cooldown; that path is gone.
    let mut remaining = params
        .max_canonical_nodes
        .saturating_sub(canonical_count(graph));
    for probe in &plan.stage3 {
        if remaining == 0 {
            break;
        }
        if concept_status(graph, probe.node) != Some(CanonizationStatus::Venerable)
            || verdicts.held.contains(&probe.node)
        {
            continue;
        }
        let Some(blast) = verdicts
            .stage3_pass
            .iter()
            .find(|&&(id, _)| id == probe.node)
            .map(|&(_, blast)| blast)
        else {
            continue;
        };
        if !scorer.admits_hop(
            graph,
            probe.node,
            CanonizationStatus::Canonical,
            params,
            now,
        ) {
            continue;
        }
        let event = promotion_event(
            graph,
            probe.node,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
            Some(narrow_blast_radius(blast)?),
            now,
        );
        outcome
            .promotions
            .push(commit_transition(graph, events, event)?);
        remaining -= 1;
    }
    // --- Budget: lowest store.blast_radius first, canonical key asc, NodeId
    // asc behind that (issue #2) ---
    // The overflow is recomputed after supersession and Stage 3, and a ranked
    // concept that is no longer Canonical (superseded above, or moved by
    // another writer) is passed over rather than counted, so the cut still
    // lands `overflow` demotions when it can.
    let mut overflow = canonical_count(graph).saturating_sub(params.max_canonical_nodes);
    for &(_, id, _) in &verdicts.demotion_ranked {
        if overflow == 0 {
            break;
        }
        if concept_status(graph, id) != Some(CanonizationStatus::Canonical) {
            continue;
        }
        overflow -= 1;
        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: plan.session.clone(),
            node_id: id,
            from_status: CanonizationStatus::Canonical,
            to_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(now),
            occurred_at: now,
            // A budget demotion still writes no reason, so its audit row is
            // byte-identical to a pre-#20 one; it reads as Budget.
            reason: None,
        };
        outcome
            .demotions
            .push(commit_transition(graph, events, event)?);
    }
    Ok(())
}

/// Phase 4 — the durable audit for every hop this cycle committed (spec §10:
/// "**every** transition goes through `store.record_canonization`").
///
/// A failure here does not un-commit anything: the transitions are in the
/// graph, in its audit, and in the write-behind log, so the flush task
/// records the same rows (deduped on event id) on its next pass. The error is
/// still surfaced — with the outcome attached — so the caller can log it.
async fn record(
    store: &dyn GraphStore,
    outcome: &EvalOutcome,
    token: Option<u64>,
) -> Result<(), StoreError> {
    for event in outcome.transitions() {
        // Fencing-token gate (#1): present the holder's token; the store
        // rejects a stale/missing one (this path had NO lease check before).
        store.record_canonization(event, token).await?;
    }
    Ok(())
}

/// Free-function form of [`Evaluator::eval_cycle`].
#[allow(clippy::too_many_arguments)] // mirrors the method; one cycle's full input
#[allow(clippy::result_large_err)] // see `Evaluator::eval_cycle`
pub async fn eval_cycle(
    evaluator: &mut Evaluator,
    graph: &RwLock<Graph>,
    store: &dyn GraphStore,
    scores: &ScoreTable,
    events: &EventSender,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<EvalOutcome, EvalError> {
    // Free-function test/utility form: no lease context, so no token is
    // presented (unleased stores permit it). The assembled loop calls the
    // `Evaluator` method directly with the holder's token.
    evaluator
        .eval_cycle(graph, store, scores, events, params, now, None)
        .await
}

/// Graph first, then emit — the commit point (see the module docs).
///
/// A failed apply does not emit.
fn commit_transition(
    graph: &mut Graph,
    events: &EventSender,
    event: CanonizationEvent,
) -> Result<CanonizationEvent, LamboError> {
    graph.apply_canonization_transition(event.clone())?;
    events::emit_canonized(events, event.clone());
    Ok(event)
}

/// Stage 1 / 2 keep the concept's current blast so apply does not wipe it.
/// Stage 3 always supplies the narrowed measurement that admitted the hop. Promotions never stamp
/// `last_demotion_time` (must not clobber a prior demotion).
fn promotion_event(
    graph: &Graph,
    node: NodeId,
    from: CanonizationStatus,
    to: CanonizationStatus,
    blast_radius: Option<i32>,
    now: DateTime<Utc>,
) -> CanonizationEvent {
    let blast_radius = match blast_radius {
        Some(b) => Some(b),
        None => match graph.node(node) {
            Some(Node::Concept(c)) => c.blast_radius,
            _ => None,
        },
    };
    CanonizationEvent {
        id: NodeId::new(),
        session_id: graph.session_id().clone(),
        node_id: node,
        from_status: from,
        to_status: to,
        blast_radius,
        last_demotion_time: None,
        occurred_at: now,
        reason: None,
    }
}

/// CON-6: never `as i32`. An unrepresentable store count is an invariant.
fn narrow_blast_radius(blast: u64) -> Result<i32, StoreError> {
    i32::try_from(blast).map_err(|_| {
        StoreError::Invariant(format!("blast_radius {blast} exceeds i32::MAX (CON-6)"))
    })
}

fn concept_status(graph: &Graph, id: NodeId) -> Option<CanonizationStatus> {
    match graph.node(id) {
        Some(Node::Concept(c)) => Some(c.canonization_status),
        _ => None,
    }
}

/// Concept ids with `status`, NodeId ascending — the ring order every cursor
/// walks.
fn ids_with_status(graph: &Graph, status: CanonizationStatus) -> Vec<NodeId> {
    let mut ids: Vec<NodeId> = graph
        .concepts()
        .filter(|c| c.canonization_status == status)
        .map(|c| c.id)
        .collect();
    ids.sort_by_key(|id| id.0);
    ids
}

fn canonical_count(graph: &Graph) -> usize {
    graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count()
}

fn score_map(scores: &ScoreTable) -> HashMap<NodeId, f64> {
    scores.ranked.iter().map(|s| (s.item, s.score)).collect()
}

fn score_lookup(map: &HashMap<NodeId, f64>, id: NodeId) -> f64 {
    map.get(&id).copied().unwrap_or(0.0)
}

#[cfg(test)]
mod tests;
