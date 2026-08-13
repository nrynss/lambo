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
//!    descending** (NodeId ascending tie-break) within the window.
//!    The cursor lives on [`Evaluator`] so the next cycle continues
//!    around the ring.
//! 4. Budget: if Canonical count exceeds `max_canonical_nodes`, demote
//!    `Canonical → None` lowest [`GraphStore::blast_radius`] first
//!    (NodeId ascending tie-break) until the count is within budget.
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
//! `now` is injected — the cycle has no wall clock, and neither do the store
//! queries it issues (see [`crate::store::GraphStore::blast_radius`]).

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use crate::canon::stage3;
use crate::canon::{stage1_candidates, stage2_passes};
use crate::daemon::events::{self, EventSender};
use crate::daemon::ScoreTable;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    CanonizationEvent, CanonizationStatus, LamboError, Node, NodeId, SessionId, StoreError,
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
    /// Budget: Canonical ids to rank for demotion. Empty unless the session
    /// is **already** over budget — Stage 3 is capped at the remaining
    /// budget, so a cycle can never create the overflow it then demotes
    /// (the phase-R2 P2 / original P1-1 property).
    demotion: Vec<NodeId>,
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
    /// `(blast, node)` for the budget ranking, blast-ascending then
    /// NodeId-ascending (spec §10: lowest blast radius demoted first).
    demotion_ranked: Vec<(u64, NodeId)>,
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
    pub async fn eval_cycle(
        &mut self,
        graph: &RwLock<Graph>,
        store: &dyn GraphStore,
        scores: &ScoreTable,
        events: &EventSender,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Result<EvalOutcome, EvalError> {
        // 1. Gather — read guard, released before the first await.
        let plan = {
            let g = graph.read();
            self.gather(&g, scores, params, now)
        };

        // 2. Verdicts — store I/O, no lock held.
        let verdicts = match verdicts(store, &plan, params, now).await {
            Ok(verdicts) => verdicts,
            // Nothing has been committed yet, so the partial outcome is empty.
            Err(err) => return Err(EvalError::new(EvalOutcome::default(), err)),
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

        // 4. Record — durable audit, no lock held.
        if let Err(err) = record(store, &outcome).await {
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
        _now: DateTime<Utc>,
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
        let stage1: Vec<NodeId> = stage1_candidates(graph, scores, params.min_peer_count)
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
        // Score-descending within the window (spec §10), NodeId ascending
        // tie-break — the evaluation order, and therefore the order `apply`
        // spends the budget in. The cursor is anchored in RING order, taken
        // above.
        let score_of = score_map(scores);
        window.sort_by(|a, b| {
            score_lookup(&score_of, *b)
                .total_cmp(&score_lookup(&score_of, *a))
                .then_with(|| a.0.cmp(&b.0))
        });
        let stage3: Vec<Stage3Probe> = window
            .into_iter()
            .map(|node| Stage3Probe {
                node,
                last_demotion_time: stage3::last_demotion_time(graph, node),
            })
            .collect();

        // Budget — probe only when the session is already over the ceiling.
        let canonicals = ids_with_status(graph, CanonizationStatus::Canonical);
        let demotion = if canonicals.len() > params.max_canonical_nodes {
            canonicals
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
    for &id in &plan.demotion {
        let blast = store
            .blast_radius(&plan.session, id, params.min_edge_age, now)
            .await?;
        out.demotion_ranked.push((blast, id));
    }
    out.demotion_ranked
        .sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
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
    for &id in &verdicts.stage2_pass {
        if concept_status(graph, id) != Some(CanonizationStatus::Candidate) {
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
    let mut remaining = params
        .max_canonical_nodes
        .saturating_sub(canonical_count(graph));
    for &(id, blast) in &verdicts.stage3_pass {
        if remaining == 0 {
            break;
        }
        if concept_status(graph, id) != Some(CanonizationStatus::Venerable) {
            continue;
        }
        let narrowed = narrow_blast_radius(blast)?;
        let event = promotion_event(
            graph,
            id,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
            Some(narrowed),
            now,
        );
        outcome
            .promotions
            .push(commit_transition(graph, events, event)?);
        remaining -= 1;
    }

    // --- Budget: lowest store.blast_radius first, NodeId asc tie-break ---
    let overflow = canonical_count(graph).saturating_sub(params.max_canonical_nodes);
    for &(_, id) in verdicts.demotion_ranked.iter().take(overflow) {
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
async fn record(store: &dyn GraphStore, outcome: &EvalOutcome) -> Result<(), StoreError> {
    for event in outcome.transitions() {
        store.record_canonization(event).await?;
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
    evaluator
        .eval_cycle(graph, store, scores, events, params, now)
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
mod tests {
    use super::*;
    use crate::types::{
        AgentId, Concept, ConceptType, DaemonEvent, Edge, EdgeType, Interaction, Scored, SessionId,
    };
    use chrono::TimeZone;
    use std::collections::HashSet;
    use uuid::Uuid;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn iid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    fn eid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(3, id))
    }

    fn interaction(id: u64, prev: Option<u64>, at: DateTime<Utc>) -> Interaction {
        Interaction {
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("i{id}")),
            previous_id: prev.map(iid),
            created_at: at,
        }
    }

    fn concept(id: u64, origin: u64, gc: i32, status: CanonizationStatus) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: iid(origin),
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: gc,
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn table(pairs: &[(u64, f64)]) -> ScoreTable {
        ScoreTable {
            epoch: 0,
            ranked: pairs
                .iter()
                .map(|&(id, score)| Scored::new(nid(id), score))
                .collect(),
        }
    }

    fn params() -> EvalParams {
        EvalParams {
            min_age: Duration::ZERO,
            min_edge_age: Duration::ZERO,
            ..EvalParams::default()
        }
    }

    fn status_of(graph: &Graph, id: NodeId) -> CanonizationStatus {
        concept_status(graph, id).expect("concept")
    }

    /// Ring arithmetic, degenerate inputs included: empty ring, zero window,
    /// a cursor past the end (wrap), and a cursor whose node has **left** the
    /// ring — the last is the churn case (F1), where the resume point must
    /// still be "the first id strictly greater", not an index.
    #[test]
    fn ring_window_is_identity_anchored_and_wraps() {
        let ring: Vec<NodeId> = (1..=5).map(nid).collect();
        assert!(ring_window(&[], None, 3).is_empty(), "empty ring");
        assert!(ring_window(&ring, None, 0).is_empty(), "zero window");
        assert_eq!(ring_window(&ring, None, 2), vec![nid(1), nid(2)]);
        assert_eq!(ring_window(&ring, Some(nid(2)), 2), vec![nid(3), nid(4)]);
        assert_eq!(
            ring_window(&ring, Some(nid(5)), 2),
            vec![nid(1), nid(2)],
            "a cursor at the end wraps to the head"
        );
        assert_eq!(
            ring_window(&ring, Some(nid(9)), 2),
            vec![nid(1), nid(2)],
            "a cursor past every member wraps too"
        );
        // Churn: 3 and 4 promoted out since the cursor was set to 3.
        let shrunk = vec![nid(1), nid(2), nid(5)];
        assert_eq!(
            ring_window(&shrunk, Some(nid(3)), 2),
            vec![nid(5), nid(1)],
            "a departed cursor resumes at the first surviving id above it"
        );
        // A window wider than the ring never repeats an id.
        let full = ring_window(&ring, Some(nid(3)), 99);
        assert_eq!(full.len(), ring.len());
        assert_eq!(
            full.iter().copied().collect::<HashSet<_>>().len(),
            ring.len(),
            "no duplicates: {full:?}"
        );
    }

    /// CON-6: `as i32` would wrap `i32::MAX + 1` to `i32::MIN`.
    #[test]
    fn blast_radius_narrow_rejects_unrepresentable_u64() {
        assert_eq!(narrow_blast_radius(0).unwrap(), 0);
        assert_eq!(narrow_blast_radius(i32::MAX as u64).unwrap(), i32::MAX);
        let err = narrow_blast_radius(i32::MAX as u64 + 1).unwrap_err();
        match err {
            StoreError::Invariant(msg) => {
                assert!(msg.contains("i32"), "{msg}");
                assert!(msg.contains("CON-6"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[cfg(feature = "store-memory")]
    mod with_store {
        use super::*;
        use crate::store::{GraphStore, MemoryStore};
        use crate::types::{Mutation, MutationBatch};

        async fn store_from_graph(graph: &Graph) -> MemoryStore {
            let store = MemoryStore::new();
            let snap = graph.snapshot();
            let mut batch = MutationBatch::new();
            for i in snap.interactions {
                batch.push(Mutation::UpsertNode {
                    node: Node::Interaction(i),
                });
            }
            for c in snap.concepts {
                batch.push(Mutation::UpsertNode {
                    node: Node::Concept(c),
                });
            }
            for e in snap.edges {
                batch.push(Mutation::UpsertEdge { edge: e });
            }
            store.flush(&batch).await.unwrap();
            store
        }

        fn channel() -> (EventSender, tokio::sync::broadcast::Receiver<DaemonEvent>) {
            crate::daemon::events::event_channel()
        }

        fn drain_canonized(
            rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
        ) -> Vec<CanonizationEvent> {
            let mut out = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(DaemonEvent::Canonized { event }) => out.push(event),
                    Ok(other) => panic!("unexpected event {other:?}"),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(e) => panic!("recv error {e:?}"),
                }
            }
            out
        }

        fn edge(id: u64, src: u64, tgt: u64, at: DateTime<Utc>) -> Edge {
            Edge {
                id: eid(id),
                session_id: sid(),
                source: nid(src),
                target: nid(tgt),
                edge_type: EdgeType::Dependency,
                weight: 1.0,
                reinforcements: 1,
                created_at: at,
                last_reinforced: at,
            }
        }

        /// `n` exclusive dependents of `hub` so `blast_radius(hub) == n`.
        fn attach_blast(graph: &mut Graph, hub: u64, n: u64, first_dep: u64) {
            let at = ts();
            for i in 0..n {
                let dep = first_dep + i;
                graph
                    .insert_concept(concept(dep, 1, 0, CanonizationStatus::None), iid(1))
                    .unwrap();
                graph.upsert_edge(edge(dep, hub, dep, at)).unwrap();
            }
        }

        #[tokio::test]
        async fn failed_apply_does_not_record_or_emit() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            g.insert_concept(concept(10, 1, 0, CanonizationStatus::None), iid(1))
                .unwrap();
            let store = store_from_graph(&g).await;
            let (tx, mut rx) = channel();

            let bad = CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: nid(10),
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(8),
                last_demotion_time: None,
                occurred_at: ts(),
            };
            let err = commit_transition(&mut g, &tx, bad).unwrap_err();
            assert!(
                err.to_string().contains("current status"),
                "fabricated apply must fail: {err}"
            );
            assert_eq!(status_of(&g, nid(10)), CanonizationStatus::None);
            assert!(g.canonization_events().is_empty());
            assert!(store
                .load_session(&sid())
                .await
                .unwrap()
                .canonization_events
                .is_empty());
            assert!(
                drain_canonized(&mut rx).is_empty(),
                "failed apply must not emit"
            );
        }

        #[tokio::test]
        async fn one_hop_per_cycle_none_with_stage2_evidence_becomes_candidate() {
            // 20 non-Canonical peers so Stage 1 opens; hub also has Stage 2
            // evidence. First cycle must stop at Candidate.
            let mut g = Graph::new(sid());
            let at = ts();
            g.insert_interaction(interaction(1, None, at)).unwrap();
            g.insert_interaction(interaction(2, Some(1), at + chrono::Duration::seconds(20)))
                .unwrap();
            g.insert_interaction(interaction(3, Some(2), at + chrono::Duration::seconds(40)))
                .unwrap();
            g.insert_interaction(interaction(4, Some(3), at + chrono::Duration::seconds(80)))
                .unwrap();

            for id in 1..=19u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            g.insert_concept(concept(20, 1, 5, CanonizationStatus::None), iid(1))
                .unwrap();
            // Distinct origins 1/2/3 covering 40/80 = 0.5 of the session.
            g.insert_concept(concept(31, 1, 0, CanonizationStatus::None), iid(1))
                .unwrap();
            g.insert_concept(concept(32, 2, 0, CanonizationStatus::None), iid(2))
                .unwrap();
            g.insert_concept(concept(33, 3, 0, CanonizationStatus::None), iid(3))
                .unwrap();
            g.upsert_edge(edge(1, 31, 20, at)).unwrap();
            g.upsert_edge(edge(2, 32, 20, at + chrono::Duration::seconds(20)))
                .unwrap();
            g.upsert_edge(edge(3, 33, 20, at + chrono::Duration::seconds(40)))
                .unwrap();

            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            assert!(
                stage2_passes(&store, &sid(), nid(20), Duration::ZERO, Utc::now())
                    .await
                    .unwrap(),
                "fixture premise: hub must clear Stage 2"
            );

            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let scores = table(&pairs);
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &g, &store, &scores, &tx, &params(), ts())
                .await
                .unwrap();

            assert_eq!(status_of(&g.read(), nid(20)), CanonizationStatus::Candidate);
            let hops: Vec<_> = outcome
                .transitions()
                .filter(|e| e.node_id == nid(20))
                .map(|e| (e.from_status, e.to_status))
                .collect();
            assert_eq!(
                hops,
                vec![(CanonizationStatus::None, CanonizationStatus::Candidate)],
                "Stage 2 evidence must not skip a stage in one tick: {hops:?}"
            );
            assert_eq!(g.read().canonization_events().len(), 1);
            assert_eq!(drain_canonized(&mut rx).len(), 1);
        }

        #[tokio::test]
        async fn budget_demotes_lowest_blast_and_records_demotion() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();

            let mut high = concept(10, 1, 5, CanonizationStatus::Canonical);
            high.blast_radius = Some(8);
            g.insert_concept(high, iid(1)).unwrap();
            attach_blast(&mut g, 10, 8, 100);

            let mut low = concept(11, 1, 5, CanonizationStatus::Canonical);
            low.blast_radius = Some(1);
            g.insert_concept(low, iid(1)).unwrap();
            attach_blast(&mut g, 11, 1, 200);

            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            assert_eq!(
                store
                    .blast_radius(&sid(), nid(10), Duration::ZERO, Utc::now())
                    .await
                    .unwrap(),
                8
            );
            assert_eq!(
                store
                    .blast_radius(&sid(), nid(11), Duration::ZERO, Utc::now())
                    .await
                    .unwrap(),
                1
            );

            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let now = ts();
            let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, now)
                .await
                .unwrap();

            assert_eq!(status_of(&g.read(), nid(10)), CanonizationStatus::Canonical);
            assert_eq!(status_of(&g.read(), nid(11)), CanonizationStatus::None);
            match g.read().node(nid(11)) {
                Some(Node::Concept(c)) => {
                    assert_eq!(c.blast_radius, None);
                    assert_eq!(c.last_demotion_time, Some(now));
                }
                other => panic!("low-blast hub must remain a concept, got {other:?}"),
            }

            assert_eq!(outcome.demotions.len(), 1);
            let d = &outcome.demotions[0];
            assert_eq!(d.node_id, nid(11));
            assert_eq!(d.from_status, CanonizationStatus::Canonical);
            assert_eq!(d.to_status, CanonizationStatus::None);
            assert_eq!(d.blast_radius, None);
            assert_eq!(d.last_demotion_time, Some(now));

            let store_snap = store.load_session(&sid()).await.unwrap();
            let graph_events = g.read().canonization_events().to_vec();
            let recorded: Vec<_> = graph_events
                .iter()
                .chain(store_snap.canonization_events.iter())
                .filter(|e| e.to_status == CanonizationStatus::None)
                .collect();
            assert_eq!(recorded.len(), 2, "graph + store each record the demotion");
            assert_eq!(drain_canonized(&mut rx).len(), 1);
        }

        #[tokio::test]
        async fn stage3_batch_is_capped_and_round_robins_score_desc() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            // 55 Venerable, NodeId order = 1..=55.
            for id in 1..=55u64 {
                g.insert_concept(concept(id, 1, 0, CanonizationStatus::Venerable), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            // Higher id → higher score, so score-desc of window 1..=50 starts at 50.
            let pairs: Vec<(u64, f64)> = (1..=55).map(|i| (i, i as f64)).collect();
            let scores = table(&pairs);
            let mut p = params();
            p.batch_size = 50;
            p.max_canonical_nodes = 10_000;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let first = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(
                first.stage3_batch.len(),
                50,
                "first cycle considers at most 50"
            );
            assert_eq!(
                first.stage3_batch[0],
                nid(50),
                "score-desc within the NodeId window 1..=50"
            );
            let first_set: HashSet<NodeId> = first.stage3_batch.iter().copied().collect();
            assert!(first_set.contains(&nid(1)) && first_set.contains(&nid(50)));
            assert!(
                !first_set.contains(&nid(51)),
                "id 51 is past the first window: {:?}",
                first.stage3_batch
            );
            assert_eq!(ev.stage3_cursor(), Some(nid(50)));

            let second = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(second.stage3_batch.len(), 50);
            let second_set: HashSet<NodeId> = second.stage3_batch.iter().copied().collect();
            assert_ne!(
                first_set, second_set,
                "second cycle must be a different window"
            );
            assert!(
                second_set.contains(&nid(51)) && second_set.contains(&nid(55)),
                "round-robin must reach the tail: {:?}",
                second.stage3_batch
            );
            // Window is 51..=55 + 1..=45; max score in that set is 55.
            assert_eq!(second.stage3_batch[0], nid(55));
            assert_eq!(ev.stage3_cursor(), Some(nid(45)));
        }

        /// `MemoryStore` whose `record_canonization` always fails, so the
        /// cycle's durable-audit phase can be exercised (F19: previously
        /// unasserted). Everything else delegates.
        struct RecordFails(MemoryStore);

        #[async_trait::async_trait]
        impl GraphStore for RecordFails {
            async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
                self.0.init_schema().await
            }
            fn capabilities(&self) -> crate::store::Capabilities {
                self.0.capabilities()
            }
            async fn flush(&self, batch: &MutationBatch) -> Result<(), crate::types::StoreError> {
                self.0.flush(batch).await
            }
            async fn load_session(
                &self,
                session: &SessionId,
            ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
                self.0.load_session(session).await
            }
            async fn keyword_candidates(
                &self,
                session: &SessionId,
                tokens: &[String],
                limit: usize,
            ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
                self.0.keyword_candidates(session, tokens, limit).await
            }
            async fn vector_candidates(
                &self,
                session: &SessionId,
                embedding: &[f32],
                limit: usize,
            ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
                self.0.vector_candidates(session, embedding, limit).await
            }
            async fn blast_radius(
                &self,
                session: &SessionId,
                node: NodeId,
                min_edge_age: Duration,
                now: DateTime<Utc>,
            ) -> Result<u64, crate::types::StoreError> {
                self.0.blast_radius(session, node, min_edge_age, now).await
            }
            async fn interaction_span(
                &self,
                session: &SessionId,
                node: NodeId,
                min_age: Duration,
                now: DateTime<Utc>,
            ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
                self.0.interaction_span(session, node, min_age, now).await
            }
            async fn record_canonization(
                &self,
                _event: &CanonizationEvent,
            ) -> Result<(), crate::types::StoreError> {
                Err(StoreError::Backend("record_canonization is down".into()))
            }
        }

        /// F3: the graph apply is the commit point. A `record_canonization`
        /// failure mid-cycle must not lose the `DaemonEvent::Canonized` (the
        /// old order emitted only *after* the durable write, so a store hiccup
        /// dropped the event forever — the flush replay re-records the row but
        /// publishes nothing), and must not discard the `EvalOutcome` naming
        /// the hops the graph has already committed.
        ///
        /// Realistic trigger: `NotFound` for a concept the flush loop has not
        /// persisted yet — the graph runs ahead of the store by one flush
        /// interval by design.
        #[tokio::test]
        async fn record_failure_keeps_the_emitted_event_and_the_partial_outcome() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = RecordFails(store_from_graph(&g).await);
            let g = RwLock::new(g);
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();

            let err = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
                .await
                .unwrap_err();
            assert!(
                err.source
                    .to_string()
                    .contains("record_canonization is down"),
                "the store error must surface: {err}"
            );
            assert_eq!(
                err.outcome
                    .transitions()
                    .map(|e| (e.node_id, e.to_status))
                    .collect::<Vec<_>>(),
                vec![(nid(20), CanonizationStatus::Candidate)],
                "the partial outcome must name the hop the graph committed"
            );
            assert_eq!(
                status_of(&g.read(), nid(20)),
                CanonizationStatus::Candidate,
                "the graph apply is the commit point; it stands"
            );
            assert_eq!(
                g.read().canonization_events().len(),
                1,
                "the in-graph audit carries the hop"
            );
            let emitted = drain_canonized(&mut rx);
            assert_eq!(
                emitted.len(),
                1,
                "the Canonized event must survive a failed durable write"
            );
            assert_eq!(emitted[0].node_id, nid(20));
            // And the write-behind log still carries it to the store later.
            assert!(!g.write().drain_log().is_empty());
        }

        /// Graph carrying only `hubs` (as Venerable) plus the seed
        /// interaction — blast radius comes from the store, so the evaluated
        /// graph needs no dependents.
        fn venerable_graph(hubs: &[u64]) -> Graph {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for &h in hubs {
                g.insert_concept(concept(h, 1, 0, CanonizationStatus::Venerable), iid(1))
                    .unwrap();
            }
            g
        }

        /// Store view of the same session: every hub with `blast` exclusive
        /// dependents, so `blast_radius(hub) == blast`.
        async fn store_with_hubs(hubs: &[(u64, u64)]) -> MemoryStore {
            let mut full = Graph::new(sid());
            full.insert_interaction(interaction(1, None, ts())).unwrap();
            for (i, &(h, blast)) in hubs.iter().enumerate() {
                full.insert_concept(concept(h, 1, 0, CanonizationStatus::Venerable), iid(1))
                    .unwrap();
                attach_blast(&mut full, h, blast, 1_000 + 100 * i as u64);
            }
            store_from_graph(&full).await
        }

        /// F1: every successful promotion removes ring members, so a
        /// **positional** cursor into the rebuilt ring skids past the
        /// longest-waiting nodes. 6 Venerables, `batch_size = 2`: cycle 1
        /// evaluates [1,2] and promotes both; cycle 2's ring is [3,4,5,6] and
        /// must resume at 3. The positional cursor computed `2 % 4 = 2` and
        /// produced [5,6] — 3 and 4 skipped, on every promoting cycle.
        #[tokio::test]
        async fn promotion_churn_does_not_skip_the_next_ring_members() {
            let hubs = [1u64, 2, 3, 4, 5, 6];
            let store = store_with_hubs(&hubs.map(|h| (h, 6))).await;
            let g = RwLock::new(venerable_graph(&hubs));
            let mut p = params();
            p.batch_size = 2;
            p.max_canonical_nodes = 10_000;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let first = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(first.stage3_batch, vec![nid(1), nid(2)]);
            assert_eq!(first.promotions.len(), 2, "both must promote (blast 6)");
            assert_eq!(ev.stage3_cursor(), Some(nid(2)));

            let second = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(
                second.stage3_batch,
                vec![nid(3), nid(4)],
                "the ring shrank under the cursor; the next window must resume \
                 at the first id after the last one evaluated"
            );
        }

        /// F1: with a steady Stage-2 inflow the positional skid repeats and
        /// the same victims are starved **forever** — the review's
        /// demonstration, reproduced.
        ///
        /// Victims 10 and 11 have blast 0, so they never promote and never
        /// leave the ring. Each cycle two fresh Venerables arrive and promote
        /// out, alternately sorting *before* and *after* the victims, which
        /// keeps a 4-element ring whose positional cursor alternates 0 → 2 →
        /// 0 → 2 and lands on the inflow every time: [1,2], [20,21], [3,4],
        /// [22,23] — the victims are never evaluated, for as long as the
        /// session keeps producing Venerables. The identity cursor reaches
        /// them on cycle 2, because "the first id after 2" is 10 whatever the
        /// ring did in between.
        #[tokio::test]
        async fn sustained_inflow_does_not_starve_the_waiting_ring_members() {
            // Everything promotes (blast 6) except the two victims.
            let store = store_with_hubs(&[
                (1, 6),
                (2, 6),
                (3, 6),
                (4, 6),
                (10, 0),
                (11, 0),
                (20, 6),
                (21, 6),
                (22, 6),
                (23, 6),
            ])
            .await;
            let g = RwLock::new(venerable_graph(&[1, 2, 10, 11]));
            let mut p = params();
            p.batch_size = 2;
            p.max_canonical_nodes = 10_000;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            // Alternating inflow, sustained: after the victims, then before,
            // then after… The alternation is what pins the positional cursor
            // to 0 → 2 → 0 → 2 while the ring stays four wide.
            let inflow = [[20u64, 21], [3, 4], [22, 23], [5, 6]];
            let mut evaluated: Vec<NodeId> = Vec::new();
            for cycle in 0..4 {
                let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                    .await
                    .unwrap();
                evaluated.extend(outcome.stage3_batch);
                if let Some(fresh) = inflow.get(cycle) {
                    let mut graph = g.write();
                    for &id in fresh {
                        graph
                            .insert_concept(
                                concept(id, 1, 0, CanonizationStatus::Venerable),
                                iid(1),
                            )
                            .unwrap();
                    }
                }
            }

            assert!(
                evaluated.contains(&nid(10)) && evaluated.contains(&nid(11)),
                "the longest-waiting Venerables must be evaluated, not starved \
                 by the inflow: {evaluated:?}"
            );
            assert!(
                evaluated.iter().filter(|&&id| id == nid(20)).count() == 1
                    && evaluated.contains(&nid(22)),
                "the inflow is still served — fairness, not victim priority: \
                 {evaluated:?}"
            );
            assert_eq!(
                status_of(&g.read(), nid(10)),
                CanonizationStatus::Venerable,
                "a blast-0 victim stays Venerable; being evaluated is the point"
            );
        }

        /// F1/F10: with the Canonical budget full the ring must not rotate at
        /// all, and `stage3_batch` must report nothing — it names the window
        /// the predicate **ran on**. The old shape took the window first and
        /// broke out of the promotion loop, so the cursor advanced over nodes
        /// that were never evaluated and the outcome claimed them anyway.
        #[tokio::test]
        async fn full_budget_evaluates_nothing_and_does_not_rotate_the_ring() {
            let store = store_with_hubs(&[(1, 6), (2, 6), (3, 6)]).await;
            let mut graph = venerable_graph(&[1, 2, 3]);
            // One pre-existing Canonical fills a budget of 1.
            let mut full = concept(10, 1, 5, CanonizationStatus::Canonical);
            full.blast_radius = Some(8);
            graph.insert_concept(full, iid(1)).unwrap();
            let g = RwLock::new(graph);

            let mut p = params();
            p.batch_size = 2;
            p.max_canonical_nodes = 1;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();
            assert!(
                outcome.stage3_batch.is_empty(),
                "no budget, so nothing was evaluated: {:?}",
                outcome.stage3_batch
            );
            assert!(outcome.promotions.is_empty());
            assert_eq!(
                ev.stage3_cursor(),
                None,
                "the ring must not rotate over nodes the cycle could not evaluate"
            );
        }

        /// F19: budget contention — `remaining == 1` with two eligible
        /// Venerables. Exactly one may promote, and spec §10's
        /// score-descending order within the batch is what decides which: the
        /// higher-scoring node wins even though it sorts later in the ring.
        /// (Ranking the window before spending the budget is what makes that
        /// true; spending it in ring order would hand the slot to the lowest
        /// NodeId instead.)
        ///
        /// R2-2: both nodes are *evaluated* — the budget cut happens in
        /// `apply`, on the nodes that passed, so `stage3_batch` names the
        /// loser too. It has to: which of them can pass is not knowable at
        /// gather time, and pre-selecting the top `remaining` starved the
        /// ring whenever the top scorer could not pass.
        #[tokio::test]
        async fn budget_contention_gives_the_last_slot_to_the_higher_score() {
            let store = store_with_hubs(&[(1, 6), (2, 6)]).await;
            let g = RwLock::new(venerable_graph(&[1, 2]));
            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            // Ring order is [1, 2]; scores put 2 first.
            let outcome = eval_cycle(
                &mut ev,
                &g,
                &store,
                &table(&[(1, 0.1), (2, 0.9)]),
                &tx,
                &p,
                ts(),
            )
            .await
            .unwrap();

            assert_eq!(
                outcome.stage3_batch,
                vec![nid(2), nid(1)],
                "the whole window is evaluated, score-descending"
            );
            assert_eq!(outcome.promotions.len(), 1);
            assert_eq!(outcome.promotions[0].node_id, nid(2));
            assert_eq!(status_of(&g.read(), nid(1)), CanonizationStatus::Venerable);
            assert_eq!(status_of(&g.read(), nid(2)), CanonizationStatus::Canonical);
            assert_eq!(canonical_count(&g.read()), 1);
        }

        /// R2-2: a top-scoring Venerable that cannot pass must not hold the
        /// last budget slot hostage.
        ///
        /// Ring `[A(score .9, blast 2), B(.5, blast 8), C(.4, blast 8)]` with
        /// `max_canonical_nodes = 1`. A fails Stage 3's `> 5`; B and C pass.
        /// While gather truncated the score-ranked window to the remaining
        /// budget, `stage3_batch` was `[A]` on all ten cycles and the budget
        /// never filled — the ring fits in `batch_size`, so the truncation
        /// re-selected the same blocked node forever. B must promote on the
        /// first cycle, and the remaining nine must then evaluate nothing
        /// (budget full).
        #[tokio::test]
        async fn a_blocked_top_scorer_does_not_starve_the_rest_of_the_ring() {
            let store = store_with_hubs(&[(1, 2), (2, 8), (3, 8)]).await;
            let g = RwLock::new(venerable_graph(&[1, 2, 3]));
            let mut p = params();
            p.max_canonical_nodes = 1;
            let scores = table(&[(1, 0.9), (2, 0.5), (3, 0.4)]);
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let mut promoted = Vec::new();
            for cycle in 0..10 {
                let outcome = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
                    .await
                    .unwrap();
                promoted.extend(outcome.promotions.iter().map(|e| e.node_id));
                if cycle == 0 {
                    assert_eq!(
                        outcome.stage3_batch,
                        vec![nid(1), nid(2), nid(3)],
                        "the whole ring is evaluated score-descending, not just \
                         the top slot's worth"
                    );
                } else {
                    assert!(
                        outcome.stage3_batch.is_empty(),
                        "budget full from cycle 1 on: {:?}",
                        outcome.stage3_batch
                    );
                }
            }

            assert_eq!(
                promoted,
                vec![nid(2)],
                "the highest-scoring node that can actually pass takes the slot"
            );
            assert_eq!(status_of(&g.read(), nid(2)), CanonizationStatus::Canonical);
            assert_eq!(
                status_of(&g.read(), nid(1)),
                CanonizationStatus::Venerable,
                "the blocked top scorer stays put — it just stops blocking"
            );
            assert_eq!(status_of(&g.read(), nid(3)), CanonizationStatus::Venerable);
        }

        /// F19: demotion ties. Two Canonicals with the **same** blast radius
        /// over a budget of 1 — spec §10 demotes the lowest blast radius
        /// first, and the documented tie-break is NodeId ascending. Without
        /// it the victim would depend on `HashMap` walk order.
        #[tokio::test]
        async fn demotion_blast_radius_tie_breaks_on_node_id() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in [20u64, 21] {
                let mut c = concept(id, 1, 5, CanonizationStatus::Canonical);
                c.blast_radius = Some(3);
                g.insert_concept(c, iid(1)).unwrap();
            }
            // Equal blast radii (3 each) — the tie-break is the only signal.
            attach_blast(&mut g, 20, 3, 100);
            attach_blast(&mut g, 21, 3, 200);

            let store = store_from_graph(&g).await;
            for id in [20u64, 21] {
                assert_eq!(
                    store
                        .blast_radius(&sid(), nid(id), Duration::ZERO, ts())
                        .await
                        .unwrap(),
                    3,
                    "fixture premise: the two hubs must tie"
                );
            }
            let g = RwLock::new(g);
            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(outcome.demotions.len(), 1);
            assert_eq!(
                outcome.demotions[0].node_id,
                nid(20),
                "a blast tie demotes the lower NodeId, deterministically"
            );
            assert_eq!(status_of(&g.read(), nid(21)), CanonizationStatus::Canonical);
        }

        /// F7 at the eval seam: `EvalParams::min_edge_age` must reach the
        /// Stage-3 blast query. The hub's dependents are attached with edges
        /// created AT `now`, so the 60s inflation guard sees blast 0 and
        /// refuses; the same graph promotes at `min_edge_age = 0`. A refactor
        /// that forwards `Duration::ZERO` (the mutant the review shipped green
        /// through both gates) fails the first half.
        #[tokio::test]
        async fn eval_forwards_min_edge_age_to_the_blast_query() {
            let store = store_with_hubs(&[(1, 6)]).await;
            let mut p = params();
            p.min_edge_age = Duration::from_secs(60);
            let (tx, _rx) = channel();

            // `now` is the instant the fixture edges were created, so every
            // dependent edge is younger than the 60s floor.
            let guarded = RwLock::new(venerable_graph(&[1]));
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &guarded, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(
                outcome.stage3_batch,
                vec![nid(1)],
                "the node is evaluated — it just must not pass"
            );
            assert!(
                outcome.promotions.is_empty(),
                "fresh edges must not inflate blast radius past the guard"
            );
            assert_eq!(
                status_of(&guarded.read(), nid(1)),
                CanonizationStatus::Venerable
            );

            // Same graph, guard off: the promotion is real, so the first half
            // above cannot pass for want of evidence.
            let open = RwLock::new(venerable_graph(&[1]));
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &open, &store, &table(&[]), &tx, &params(), ts())
                .await
                .unwrap();
            assert_eq!(outcome.promotions.len(), 1);
            assert_eq!(
                status_of(&open.read(), nid(1)),
                CanonizationStatus::Canonical
            );
        }

        #[tokio::test]
        async fn emit_canonized_reaches_event_sender_subscriber() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
                .await
                .unwrap();

            let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("Canonized within 1s")
                .unwrap();
            match got {
                DaemonEvent::Canonized { event } => {
                    assert_eq!(event.node_id, nid(20));
                    assert_eq!(event.from_status, CanonizationStatus::None);
                    assert_eq!(event.to_status, CanonizationStatus::Candidate);
                    assert!(event.last_demotion_time.is_none());
                }
                other => panic!("expected Canonized, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn audit_rows_equal_committed_transitions() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
                .await
                .unwrap();

            let committed: Vec<_> = outcome.transitions().cloned().collect();
            assert!(!committed.is_empty());
            assert_eq!(g.read().canonization_events(), committed.as_slice());
            assert_eq!(
                store
                    .load_session(&sid())
                    .await
                    .unwrap()
                    .canonization_events,
                committed
            );
            assert_eq!(drain_canonized(&mut rx), committed);
        }

        #[tokio::test]
        async fn stage3_promotion_capped_at_remaining_budget() {
            // P2 (phase R2): a cycle must never push the Canonical count over
            // max_canonical_nodes. A Venerable that would overflow stays
            // Venerable; the pre-existing Canonical is not displaced and no
            // same-tick promote-then-demote occurs (the original P1-1).
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();

            // Pre-existing Canonical, blast 8.
            let mut existing = concept(10, 1, 5, CanonizationStatus::Canonical);
            existing.blast_radius = Some(8);
            g.insert_concept(existing, iid(1)).unwrap();
            attach_blast(&mut g, 10, 8, 100);

            // Venerable that would clear Stage 3 (blast 6 > 5).
            let venerable = concept(20, 1, 5, CanonizationStatus::Venerable);
            g.insert_concept(venerable, iid(1)).unwrap();
            attach_blast(&mut g, 20, 6, 200);

            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();

            assert_eq!(
                status_of(&g.read(), nid(20)),
                CanonizationStatus::Venerable,
                "budget full: the Venerable must wait, not overflow"
            );
            assert_eq!(
                status_of(&g.read(), nid(10)),
                CanonizationStatus::Canonical,
                "the pre-existing Canonical is untouched when not over budget"
            );
            assert!(outcome.promotions.is_empty(), "no promotion over budget");
            assert!(outcome.demotions.is_empty(), "no demotion at exact budget");
            assert_eq!(canonical_count(&g.read()), 1, "count stays within budget");
            assert!(
                drain_canonized(&mut rx).is_empty(),
                "no transitions committed"
            );
        }

        #[tokio::test]
        async fn flush_after_eval_does_not_duplicate_audit_rows() {
            // P1-2: record_canonization (immediate) + write-behind flush must not
            // double the demo audit trail. Reload after drain_log+flush must carry
            // each committed transition exactly once.
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let g = RwLock::new(g);
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
                .await
                .unwrap();
            let committed: Vec<_> = outcome.transitions().cloned().collect();
            assert_eq!(committed.len(), 1, "one None→Candidate hop");

            // Replay the write-behind log (the same transition as the live write).
            let batch = g.write().drain_log();
            assert!(!batch.is_empty());
            store.flush(&batch).await.unwrap();

            let reloaded = store
                .load_session(&sid())
                .await
                .unwrap()
                .canonization_events;
            assert_eq!(
                reloaded.len(),
                committed.len(),
                "flush must not duplicate the audit trail"
            );
            assert_eq!(reloaded, committed, "reloaded audit matches committed hops");
        }
    }

    #[cfg(all(feature = "store-memory", feature = "fixtures"))]
    mod fixture {
        use super::*;
        use crate::config::ScoringWeights;
        use crate::daemon::score::rescore;
        use crate::store::GraphStore;

        fn rest_sid() -> SessionId {
            SessionId::from("session-rest-api")
        }

        /// Injected clock for the `session-rest-api` fixture (F8): every
        /// planted timestamp is 2026-08-10T09:00–09:55Z, so the cycle's `now`
        /// must sit *after* the session — the store adapters age their cutoffs
        /// against the caller's clock, and the module-level `ts()` predates the
        /// fixture by a year (it would cut every structural edge away).
        fn fixture_now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap()
        }

        fn rewind_canonicals(snap: &mut crate::types::GraphSnapshot) {
            for c in snap.concepts.iter_mut() {
                if c.canonization_status == CanonizationStatus::Canonical {
                    c.canonization_status = CanonizationStatus::None;
                    c.blast_radius = None;
                }
            }
        }

        fn find_id(graph: &Graph, content: &str) -> NodeId {
            graph
                .concepts()
                .find(|c| c.content == content)
                .unwrap_or_else(|| panic!("{content} present"))
                .id
        }

        fn hops_for<'a>(
            events: impl IntoIterator<Item = &'a CanonizationEvent>,
            id: NodeId,
        ) -> Vec<(CanonizationStatus, CanonizationStatus)> {
            events
                .into_iter()
                .filter(|e| e.node_id == id)
                .map(|e| (e.from_status, e.to_status))
                .collect()
        }

        #[tokio::test]
        async fn rest_api_user_schema_progresses_three_hops_with_audit() {
            let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
            rewind_canonicals(&mut snap);
            let graph = Graph::from_snapshot(snap.clone()).unwrap();
            let store = crate::store::MemoryStore::new();
            store.seed(snap).unwrap();

            let us = find_id(&graph, "user schema");
            assert_eq!(status_of(&graph, us), CanonizationStatus::None);

            let scores = ScoreTable {
                epoch: graph.epoch(),
                ranked: rescore(&graph, &ScoringWeights::default()),
            };
            let graph = RwLock::new(graph);
            let mut p = params();
            p.min_peer_count = crate::Config::default().canonization_min_peer_count;

            let (tx, mut rx) = crate::daemon::events::event_channel();
            let mut ev = Evaluator::new();
            let mut all_committed = Vec::new();
            for _ in 0..3 {
                let outcome = eval_cycle(&mut ev, &graph, &store, &scores, &tx, &p, fixture_now())
                    .await
                    .unwrap();
                all_committed.extend(outcome.transitions().cloned());
            }

            assert_eq!(status_of(&graph.read(), us), CanonizationStatus::Canonical);
            match graph.read().node(us) {
                Some(Node::Concept(c)) => {
                    assert_eq!(c.blast_radius, Some(8), "Stage 3 must stamp measured blast");
                    assert!(c.last_demotion_time.is_none());
                }
                other => panic!("user schema must be a concept, got {other:?}"),
            }

            let graph_events = graph.read().canonization_events().to_vec();
            let us_hops = hops_for(&graph_events, us);
            assert_eq!(
                us_hops,
                vec![
                    (CanonizationStatus::None, CanonizationStatus::Candidate),
                    (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
                    (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
                ],
                "one row per hop; a skipped audit would drop a pair: {us_hops:?}"
            );
            assert_eq!(
                hops_for(&all_committed, us),
                us_hops,
                "outcome transitions must match the in-graph audit"
            );
            let store_hops = hops_for(
                &store
                    .load_session(&rest_sid())
                    .await
                    .unwrap()
                    .canonization_events,
                us,
            );
            assert_eq!(store_hops, us_hops, "store audit must have one row per hop");

            let emitted = {
                let mut out = Vec::new();
                while let Ok(DaemonEvent::Canonized { event }) = rx.try_recv() {
                    out.push(event);
                }
                out
            };
            assert_eq!(
                hops_for(&emitted, us),
                us_hops,
                "emit_canonized must fire once per hop"
            );
        }

        #[tokio::test]
        async fn rest_api_api_layer_reaches_venerable_never_canonical() {
            let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
            let api_id = snap
                .concepts
                .iter()
                .find(|c| c.content == "api layer")
                .expect("api layer present")
                .id;
            {
                let api = snap
                    .concepts
                    .iter_mut()
                    .find(|c| c.id == api_id)
                    .expect("api layer present");
                // Start at Candidate so Stage 2 is the first hop; blast 1
                // must refuse Canonical.
                api.canonization_status = CanonizationStatus::Candidate;
            }
            let graph = Graph::from_snapshot(snap.clone()).unwrap();
            let store = crate::store::MemoryStore::new();
            store.seed(snap).unwrap();
            let scores = ScoreTable {
                epoch: graph.epoch(),
                ranked: rescore(&graph, &ScoringWeights::default()),
            };
            let graph = RwLock::new(graph);
            let (tx, _rx) = crate::daemon::events::event_channel();
            let mut ev = Evaluator::new();
            for _ in 0..3 {
                eval_cycle(
                    &mut ev,
                    &graph,
                    &store,
                    &scores,
                    &tx,
                    &params(),
                    fixture_now(),
                )
                .await
                .unwrap();
            }
            assert_eq!(
                status_of(&graph.read(), api_id),
                CanonizationStatus::Venerable
            );
            let graph_events = graph.read().canonization_events().to_vec();
            let hops = hops_for(&graph_events, api_id);
            assert!(
                hops.iter().any(|&(f, t)| f == CanonizationStatus::Candidate
                    && t == CanonizationStatus::Venerable),
                "api layer must hop to Venerable: {hops:?}"
            );
            assert!(
                hops.iter()
                    .all(|&(_, t)| t != CanonizationStatus::Canonical),
                "api layer blast=1 must never become Canonical: {hops:?}"
            );
        }

        /// Exit criterion "same test green against SQLite once T3.6 lands":
        /// the stage predicates and eval cycle are store-agnostic; running the
        /// three-hop progression against SqliteStore proves the SQL structural
        /// queries (blast_radius, interaction_span) yield the same verdict.
        #[cfg(feature = "store-sqlite")]
        #[tokio::test]
        async fn sqlite_three_hop_progression_matches_memory() {
            use crate::store::{GraphStore, SqliteStore};

            let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
            rewind_canonicals(&mut snap);
            let graph = Graph::from_snapshot(snap.clone()).unwrap();
            let store = SqliteStore::connect("sqlite::memory:").unwrap();
            store.init_schema().await.unwrap();
            store.seed(&snap).await.unwrap();

            let us = find_id(&graph, "user schema");
            assert_eq!(status_of(&graph, us), CanonizationStatus::None);

            let scores = ScoreTable {
                epoch: graph.epoch(),
                ranked: rescore(&graph, &ScoringWeights::default()),
            };
            let graph = RwLock::new(graph);
            let mut p = params();
            p.min_peer_count = crate::Config::default().canonization_min_peer_count;

            let (tx, _rx) = crate::daemon::events::event_channel();
            let mut ev = Evaluator::new();
            for i in 0..3 {
                // Advance the clock per cycle as production does, so each hop
                // gets a distinct occurred_at and the SQL audit orders by it.
                let now = fixture_now() + chrono::Duration::seconds(60 * i as i64);
                eval_cycle(&mut ev, &graph, &store, &scores, &tx, &p, now)
                    .await
                    .unwrap();
            }
            assert_eq!(status_of(&graph.read(), us), CanonizationStatus::Canonical);

            let graph_events = graph.read().canonization_events().to_vec();
            let us_hops = hops_for(&graph_events, us);
            assert_eq!(
                us_hops,
                vec![
                    (CanonizationStatus::None, CanonizationStatus::Candidate),
                    (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
                    (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
                ],
                "SQLite structural queries must yield the same progression: {us_hops:?}"
            );

            let reloaded = store.load_session(&rest_sid()).await.unwrap();
            assert_eq!(
                hops_for(&reloaded.canonization_events, us),
                us_hops,
                "SQLite canonization_events must match the in-graph audit"
            );
        }
    }
}
