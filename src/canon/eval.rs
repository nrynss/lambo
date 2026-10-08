//! Evaluation cycle — promotions, budget demotion, audit (T6.4, spec §10).
//!
//! One hop per cycle (documented): a node may take **at most one** legal
//! edge of the state machine per tick. The eval loop never uses
//! `None → Venerable` (that edge is reserved for `set_root_goal`). The
//! hops this cycle will emit, in order, are:
//!
//! 1. Stage 1: `None → Candidate` for nodes still-None in
//!    [`stage1_candidates`].
//! 2. Stage 2: `Candidate → Venerable` for nodes that were already
//!    Candidate *before* this cycle's Stage 1 hop (a node that just
//!    became Candidate is not re-checked for Venerable in the same tick).
//! 3. Stage 3: `Venerable → Canonical` for a **round-robin** window of
//!    at most [`EvalParams::batch_size`] Venerable nodes, **score-
//!    descending** (canonical key ascending, then NodeId ascending —
//!    [`tie_break_by_key`], issue #2) within the window.
//!    The cursor lives on [`Evaluator`] so the next cycle continues
//!    around the ring.
//! 4. Budget: if Canonical count exceeds `max_canonical_nodes`, demote
//!    `Canonical → None` lowest [`GraphStore::blast_radius`] first
//!    (canonical key ascending, then NodeId ascending —
//!    [`tie_break_by_key`], issue #2) until the count is within budget.
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
//! 2. [`verdicts`] — **async, no lock.** One `interaction_span` per Stage-2
//!    window member and one `blast_radius` per Stage-3 window member /
//!    budget probe. Touches no graph.
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
use crate::canon::{stage2_passes, PromotionPolicy};
use crate::daemon::events::{self, EventSender};
use crate::daemon::ScoreTable;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    tie_break_by_key, CanonizationEvent, CanonizationStatus, LamboError, Node, NodeId, SessionId,
    StoreError,
};

/// Round-robin cursors plus the one-cycle write path.
#[derive(Clone, Debug, Default)]
pub struct Evaluator {
    /// Last Stage-2 Candidate evaluated; the next cycle resumes after it.
    stage2_cursor: Option<NodeId>,
    /// Last Stage-3 Venerable evaluated; the next cycle resumes after it.
    stage3_cursor: Option<NodeId>,
}

/// Knobs for one [`eval_cycle`]. Defaults match [`crate::Config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalParams {
    pub min_peer_count: usize,
    /// Forwarded to Stage 2 (`interaction_span` age floor).
    pub min_age: Duration,
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
}

/// What one cycle wrote. `promotions` then `demotions` is the commit order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvalOutcome {
    pub promotions: Vec<CanonizationEvent>,
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
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            min_peer_count: config.canonization_min_peer_count,
            min_age: config.canonization_edge_min_age,
            min_edge_age: config.canonization_edge_min_age,
            cooldown: config.canonization_repromotion_cooldown,
            batch_size: config.canonization_eval_batch_size,
            max_canonical_nodes: config.max_canonical_nodes,
            promotion_policy: config.promotion_policy,
        }
    }
}

impl Default for EvalParams {
    fn default() -> Self {
        Self::from_config(&crate::Config::default())
    }
}

impl EvalOutcome {
    /// Promotions followed by demotions — every hop this cycle committed.
    pub fn transitions(&self) -> impl Iterator<Item = &CanonizationEvent> {
        self.promotions.iter().chain(self.demotions.iter())
    }

    /// Whether this cycle committed nothing (the steady state).
    pub fn is_empty(&self) -> bool {
        self.promotions.is_empty() && self.demotions.is_empty()
    }
}

/// Everything one cycle reads from the graph, captured under a single read
/// guard so the verdict phase can run with no lock held.
#[derive(Clone, Debug, PartialEq)]
struct CyclePlan {
    session: SessionId,
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
    #[allow(clippy::too_many_arguments)] // one cycle's full input; mirrors eval_cycle()
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

        // The P90 population is the graph's, the scores are the daemon's. A
        // table older than the graph drags every concept born since the last
        // rescore into the peer distribution at 0.0, which inflates `n` and
        // floods the bottom of the P90 population. The daemon's rescore is
        // epoch-gated and runs on its own (1s) tick while this cycle runs on
        // a 60s one, so a brief lag is expected rather than a fault: report
        // it and proceed. (Recall refuses a stale table only because it must
        // not *cache* a compute keyed on the graph epoch.)
        if scores.epoch != graph.epoch() {
            tracing::debug!(
                target: "lambo::canon",
                scores_epoch = scores.epoch,
                graph_epoch = graph.epoch(),
                "canonization cycle running on a score table older than the graph"
            );
        }

        // Stage 1 — still-None candidates, NodeId ascending. No cursor: the
        // set drains (a promoted node leaves it), unlike Stage 2, whose
        // members can fail their evidence gate cycle after cycle.
        //
        // C1: the predicate is chosen by `promotion_policy` rather than
        // welded in. The default arm is `SwarmScorer`, which forwards to
        // `stage1_candidates` with the same argument this line always passed —
        // the seam adds an indirection and nothing else. `now` is handed to
        // the scorer even though swarm ignores it, so D2 can give the solo
        // policy event time without widening the trait.
        let stage1: Vec<NodeId> = params
            .promotion_policy
            .scorer()
            .candidates(graph, scores, params, now)
            .into_iter()
            .filter(|&id| concept_status(graph, id) == Some(CanonizationStatus::None))
            .take(params.batch_size)
            .collect();

        // Stage 2 — one `interaction_span` round-trip per member, so the
        // window is capped and walks the identity cursor (F13).
        let candidates = ids_with_status(graph, CanonizationStatus::Candidate);
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
        let remaining = params
            .max_canonical_nodes
            .saturating_sub(canonical_count(graph));
        let venerable = ids_with_status(graph, CanonizationStatus::Venerable);
        let mut window = if remaining == 0 {
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
fn ring_window(ring: &[NodeId], cursor: Option<NodeId>, size: usize) -> Vec<NodeId> {
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
    let mut out = Verdicts::default();
    for &id in &plan.stage2 {
        if stage2_passes(store, &plan.session, id, params.min_age, now).await? {
            out.stage2_pass.push(id);
        }
    }
    for probe in &plan.stage3 {
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
    Ok(out)
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

    // Every promotion hop's admission goes through the policy seam: the stage
    // predicates stay policy-independent measures, and the active policy
    // decides what their verdict is worth (C-R1-1). Swarm keeps the verdict
    // as the whole decision — byte-for-byte the pre-seam pipeline. Solo
    // substitutes its §3.2 score bands, which is what makes the published
    // Venerable/Canonical bars drive the ladder.
    let scorer = params.promotion_policy.scorer();

    // --- Stage 1: None → Candidate (one hop; no skip to Venerable) ---
    for &id in &plan.stage1 {
        if concept_status(graph, id) != Some(CanonizationStatus::None) {
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
    // The window is walked in ring order; a member promotes iff its policy
    // admits the hop given the span verdict. Under swarm that reduces to the
    // verdict alone (the pre-seam set), so membership and order are unchanged.
    for &id in &plan.stage2 {
        if concept_status(graph, id) != Some(CanonizationStatus::Candidate) {
            continue;
        }
        let evidence = verdicts.stage2_pass.contains(&id);
        if !scorer.admits_hop(graph, id, CanonizationStatus::Venerable, evidence) {
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
    // Admission again goes through the policy seam. A node whose blast-radius
    // verdict passed keeps the measurement that admitted it — the audit row is
    // stamped with exactly that value (F9). A node admitted *without* a
    // passing verdict (solo's score bands) has no measurement to stamp and is
    // cooldown-gated here instead: its admission bypassed the verdict phase,
    // where that gate normally runs.
    let mut remaining = params
        .max_canonical_nodes
        .saturating_sub(canonical_count(graph));
    for probe in &plan.stage3 {
        if remaining == 0 {
            break;
        }
        if concept_status(graph, probe.node) != Some(CanonizationStatus::Venerable) {
            continue;
        }
        let evidence = verdicts
            .stage3_pass
            .iter()
            .find(|&&(id, _)| id == probe.node)
            .map(|&(_, blast)| blast);
        if evidence.is_none()
            && stage3::in_repromotion_cooldown(probe.last_demotion_time, params.cooldown, now)
        {
            continue;
        }
        if !scorer.admits_hop(
            graph,
            probe.node,
            CanonizationStatus::Canonical,
            evidence.is_some(),
        ) {
            continue;
        }
        let narrowed = match evidence {
            Some(blast) => Some(narrow_blast_radius(blast)?),
            // Score-admitted: keep the concept's current blast (promotion_event).
            None => None,
        };
        let event = promotion_event(
            graph,
            probe.node,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
            narrowed,
            now,
        );
        outcome
            .promotions
            .push(commit_transition(graph, events, event)?);
        remaining -= 1;
    }
    // --- Budget: lowest store.blast_radius first, canonical key asc, NodeId
    // asc behind that (issue #2) ---
    let overflow = canonical_count(graph).saturating_sub(params.max_canonical_nodes);
    for &(_, id, _) in verdicts.demotion_ranked.iter().take(overflow) {
        if concept_status(graph, id) != Some(CanonizationStatus::Canonical) {
            continue;
        }
        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: plan.session.clone(),
            node_id: id,
            from_status: CanonizationStatus::Canonical,
            to_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(now),
            occurred_at: now,
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
/// Stage 3 supplies the narrowed measurement. Promotions never stamp
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
