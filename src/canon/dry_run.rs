//! The read-only canonization dry run (#36 PR 5): what the evaluator would
//! do to a session, computed on an in-memory copy and never persisted.
//!
//! [`dry_run`] takes a [`GraphSnapshot`] (the CLI loads it from a private
//! copy of a SQLite store) and, for one policy:
//!
//! 1. **Surveys the start**: every concept's next hop explained with the
//!    evaluator's own predicates ([`crate::canon::explain_admission`]), so
//!    each unmet gate is counted per stage, unbatched.
//! 2. **Runs the real evaluator** ([`Evaluator::eval_cycle`]) on an overlay:
//!    a [`Graph`] built from the snapshot and a [`MemoryStore`] seeded with
//!    it. The clock is injected and advances [`DryRunParams::step`] a cycle;
//!    scores are recomputed before every cycle, as the daemon does.
//! 3. **Stops deterministically** (see [`quiet_needed`] and [`cooling`]):
//!    after enough consecutive empty cycles to cover the longest Stage 2/3
//!    ring at the batch size plus three, counted only once no re-promotion
//!    cooldown is running, or at [`DryRunParams::max_cycles`], which fails
//!    the `fixed_point` invariant.
//! 4. **Surveys the fixed point** the same way, lists the concepts one gate
//!    away and the load-bearing concepts Stage 2 still stops, and checks the
//!    run's invariants.
//!
//! # What it never does
//!
//! It takes, renews or steals no lease: the overlay store holds no lease row,
//! so the evaluator's writes there need no fencing token, and the copied
//! source's lease row is only read and reported (by the CLI). It writes
//! nothing outside the overlay, which is dropped when the run returns, and it
//! persists no proposal. It reports ids, short type and status labels and
//! counts, never concept text.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::canon::stage3::in_repromotion_cooldown;
use crate::canon::{
    admission_inputs, explain_admission, stage2_evidence_passes, Admission, EvalParams, Evaluator,
    PromotionPolicy,
};
use crate::config::ScoringWeights;
use crate::daemon::score::rescore;
use crate::graph::Graph;
use crate::store::{GraphStore, MemoryStore};
use crate::types::{
    CanonizationEvent, CanonizationStatus, ConceptType, GraphSnapshot, LamboError, Mutation,
    MutationBatch, Node, NodeId, ScoreTable, SessionId,
};

#[path = "dry_run_report.rs"]
mod report;
pub(crate) use report::policy_delta as report_delta;
pub use report::{
    baseline_mismatch, compare, render_comparison, render_report, Comparison, PolicyDelta,
};

/// One policy's report as text under a heading (the alias preview's
/// verbose sections).
pub(crate) fn render_policy_for_preview(out: &mut String, title: &str, p: &PolicyReport) {
    out.push_str(&format!("\n-- {title} --"));
    report::render_policy(out, p);
}

/// The report format version, bumped when a field changes meaning.
pub const REPORT_VERSION: u32 = 2;

pub use super::dry_run_defaults::{
    DEFAULT_MAX_CYCLES, DEFAULT_MIN_QUIET, DEFAULT_STEP, DEFAULT_TOP,
};

/// The knobs of one dry run.
#[derive(Clone, Debug)]
pub struct DryRunParams {
    /// The evaluator's parameters; `promotion_policy` picks the policy.
    pub eval: EvalParams,
    /// The evaluation clock at the first cycle.
    pub now: DateTime<Utc>,
    /// How far the clock advances per cycle.
    pub step: Duration,
    /// The fewest consecutive empty cycles that end the run.
    pub min_quiet_cycles: usize,
    /// The safety limit.
    pub max_cycles: usize,
    /// How many ids each list carries.
    pub top: usize,
    /// Raise every `gc_survived` below 3 to 3 first: the state after three
    /// GC sweeps (Swarm's Stage 1 needs it; Solo does not read it).
    pub assume_survived: bool,
    /// Concepts reported by id at the start and the stop whatever their
    /// state (the fair test's named rulings, say); see [`resolve_watch`].
    pub watch: Vec<NodeId>,
    /// The composite score's weights, which order Stage 1's queue and the
    /// Stage 3 window. `lambo.toml` cannot set them, so the CLI always
    /// uses [`crate::Config`]'s defaults, which are what every writer runs;
    /// a library caller with its own `Config` passes `config.scoring`
    /// (review L7).
    pub weights: ScoringWeights,
}

impl DryRunParams {
    /// Defaults for `policy` at `now`.
    pub fn new(policy: PromotionPolicy, now: DateTime<Utc>) -> Self {
        Self {
            eval: EvalParams {
                promotion_policy: policy,
                ..EvalParams::default()
            },
            now,
            step: DEFAULT_STEP,
            min_quiet_cycles: DEFAULT_MIN_QUIET,
            max_cycles: DEFAULT_MAX_CYCLES,
            top: DEFAULT_TOP,
            assume_survived: false,
            watch: Vec::new(),
            weights: crate::Config::default().scoring,
        }
    }
}

/// Status-by-type counts: `status -> type -> count`.
pub type Census = BTreeMap<String, BTreeMap<String, usize>>;

/// A count split by type, with the Resource share named (#36's artifact
/// question).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Split {
    /// All of them.
    pub total: usize,
    /// How many are Resources.
    pub resource: usize,
    /// By type label.
    pub by_type: BTreeMap<String, usize>,
}

impl Split {
    fn add(&mut self, t: ConceptType) {
        self.total += 1;
        if t == ConceptType::Resource {
            self.resource += 1;
        }
        *self.by_type.entry(label(t)).or_default() += 1;
    }
}

/// One concept in a list: id and labels, never text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdRow {
    /// The node id.
    pub id: NodeId,
    /// Type label.
    pub concept_type: ConceptType,
    /// Status label.
    pub status: CanonizationStatus,
    /// The next status, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<CanonizationStatus>,
    /// Aged dependents (Stage 3's blast radius).
    pub dependents: u64,
    /// Stage 2's distinct other interactions, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<u64>,
    /// Stage 2's capped coverage, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    /// The next hop's unmet gates (`stage/gate`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<String>,
}

/// Every concept's next hop at one instant, aggregated.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Survey {
    /// Concepts whose next hop's gates all hold, by hop (`None->Candidate`).
    pub eligible: BTreeMap<String, Split>,
    /// Unmet gates, by hop and gate (`None->Candidate stage1/gc_survived`).
    /// A concept failing two gates counts under both.
    pub unmet: BTreeMap<String, usize>,
    /// Concepts whose next hop fails exactly one gate and that have at least
    /// one dependent (or are Venerable), by hop and gate.
    pub one_gate_away: BTreeMap<String, Split>,
    /// The first [`DryRunParams::top`] of them, most dependents first.
    pub one_gate_away_top: Vec<IdRow>,
    /// Concepts with more than five dependents (Stage 3's bar) that are not
    /// Canonical, and why they stop.
    pub load_bearing: Vec<IdRow>,
    /// How many there are (the list is capped).
    pub load_bearing_total: usize,
    /// The [`DryRunParams::watch`] concepts, in the order given, with their
    /// next hop's unmet gates and the Stage 2 gates that stop them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched: Vec<IdRow>,
}

/// Resolve `--watch` references against `snap`: a full node id, or an
/// 8-hex short id that starts exactly one concept's id.
pub fn resolve_watch(snap: &GraphSnapshot, refs: &[String]) -> Result<Vec<NodeId>, String> {
    use crate::surface::resolve::{id_has_hex_prefix, is_short_id_shaped, parse_full_id};
    refs.iter()
        .enumerate()
        .map(|(i, r)| {
            let r = r.trim();
            if let Some(id) = parse_full_id(r)
                && r.len() == 36
            {
                return snap
                    .concepts
                    .iter()
                    .any(|c| c.id == id)
                    .then_some(id)
                    .ok_or_else(|| format!("--watch {r}: no such concept in the session"));
            }
            if !is_short_id_shaped(r) {
                return Err(format!(
                    "--watch argument {}: expected a node id or an 8-character short id \
                     (the argument is not repeated here, since it may be concept text)",
                    i + 1
                ));
            }
            let found: Vec<NodeId> = snap
                .concepts
                .iter()
                .filter(|c| id_has_hex_prefix(c.id.0, r))
                .map(|c| c.id)
                .collect();
            match found.as_slice() {
                [one] => Ok(*one),
                [] => Err(format!("--watch {r}: no such concept in the session")),
                _ => Err(format!(
                    "--watch {r}: {} concepts share this short id; pass the full id",
                    found.len()
                )),
            }
        })
        .collect()
}

/// Why the run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Enough consecutive empty cycles (see [`quiet_needed`]).
    FixedPoint,
    /// [`DryRunParams::max_cycles`] ran out first.
    CycleLimit,
}

/// One named invariant and whether it held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invariant {
    /// Stable name.
    pub name: String,
    /// Whether it held.
    pub held: bool,
    /// Ids or counts, never text.
    pub detail: String,
}

impl Invariant {
    pub(crate) fn new(name: &str, held: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            held,
            detail: detail.into(),
        }
    }
}

/// One policy's dry run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyReport {
    /// The policy.
    pub policy: PromotionPolicy,
    /// Statuses before the run.
    pub start: Census,
    /// The start, explained.
    pub first_evaluation: Survey,
    /// Cycles run.
    pub cycles: usize,
    /// Why it stopped.
    pub stop: StopReason,
    /// The empty-cycle run that ended it.
    pub quiet_needed: usize,
    /// Whether the Canonical budget was full at the stop.
    pub budget_saturated: bool,
    /// Hops taken in the first cycle, by hop and Resource split.
    pub first_cycle: BTreeMap<String, usize>,
    /// Every hop taken, by hop and Resource split.
    pub hops: BTreeMap<String, usize>,
    /// Concepts that reached each status during the run (a concept that
    /// climbed two rungs counts once under each), by type.
    pub reached: BTreeMap<String, Split>,
    /// Concepts that ended lower than they started (budget demotion).
    pub demoted: Split,
    /// Statuses at the stop.
    pub end: Census,
    /// The stop, explained.
    pub fixed_point: Survey,
    /// Canonicals at the stop, most dependents first (capped).
    pub canonical: Vec<IdRow>,
    /// How many Canonicals at the stop.
    pub canonical_total: usize,
    /// Every Canonical's id at the stop, ascending: what `--baseline`
    /// compares (review L2; `canonical` is capped at `top`).
    #[serde(default)]
    pub canonical_ids: Vec<NodeId>,
    /// The run's checks.
    pub invariants: Vec<Invariant>,
}

/// Facts about the source the CLI read, filled in by the caller.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceFacts {
    /// The input path as given.
    pub path: String,
    /// SHA-256 of the input (and its `-wal`, when present) before the run.
    pub sha256_before: String,
    /// The same after the run.
    pub sha256_after: String,
    /// Whether a lease row was copied with the store.
    pub lease_present: bool,
    /// The copied row's fencing token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_token: Option<u64>,
    /// Whether the copied lease was still unexpired at the evaluation clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_live_at_clock: Option<bool>,
    /// Durable write intents a writer would still replay; the dry run does
    /// not apply them.
    pub unconsumed_intents: usize,
}

/// A whole dry run: the input, the knobs and one report per policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DryRunReport {
    /// [`REPORT_VERSION`].
    pub version: u32,
    /// The session.
    pub session: SessionId,
    /// The evaluation clock at the first cycle.
    pub at: DateTime<Utc>,
    /// Concepts, interactions, edges.
    pub concepts: usize,
    /// Interactions in the session.
    pub interactions: usize,
    /// Edges in the session.
    pub edges: usize,
    /// Survival counts raised to 3 by `assume_survived`.
    pub survival_raised: usize,
    /// Whether the run had `canonization_supersede` on (#20). Absent when
    /// off, so a report of a default run is unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supersede: bool,
    /// Rows dated after the clock (review M1); any fails `clock_after_the_data`.
    pub rows_after_clock: RowsAfterClock,
    /// The source, when the CLI read one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceFacts>,
    /// One per policy.
    pub policies: Vec<PolicyReport>,
    /// Checks over the whole run (the CLI adds `source_unchanged`).
    pub invariants: Vec<Invariant>,
    /// The comparison with a baseline report, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<Comparison>,
}

/// A type's short label.
pub fn label(t: ConceptType) -> String {
    format!("{t:?}")
}

fn status_label(s: CanonizationStatus) -> String {
    format!("{s:?}")
}

fn hop_label(from: CanonizationStatus, to: Option<CanonizationStatus>) -> String {
    match to {
        Some(to) => format!("{from:?}->{to:?}"),
        None => format!("{from:?}"),
    }
}

fn resource_split(t: ConceptType) -> &'static str {
    if t == ConceptType::Resource {
        "Resource"
    } else {
        "non-Resource"
    }
}

fn census(g: &Graph) -> Census {
    let mut out = Census::new();
    for c in g.concepts() {
        *out.entry(status_label(c.canonization_status))
            .or_default()
            .entry(label(c.concept_type))
            .or_default() += 1;
    }
    out
}

fn rank(s: CanonizationStatus) -> u8 {
    match s {
        CanonizationStatus::None => 0,
        CanonizationStatus::Candidate => 1,
        CanonizationStatus::Venerable => 2,
        CanonizationStatus::Canonical => 3,
    }
}

/// Prepare `snap` for an overlay: raise survival when asked, drop vectors
/// (no gate reads them) and write intents (a writer's replay, not graph
/// state). Returns how many survival counts were raised.
pub fn prepare(snap: &mut GraphSnapshot, assume_survived: bool) -> usize {
    let mut raised = 0;
    for c in snap.concepts.iter_mut() {
        c.embedding = None;
        if assume_survived && c.gc_survived < 3 {
            c.gc_survived = 3;
            raised += 1;
        }
    }
    snap.write_intents.clear();
    raised
}

/// The overlay: a graph and a store holding the same copy, both private to
/// the run. The store is filled through its ordinary `flush`, unfenced: it
/// is fresh, so it has never been leased.
async fn overlay(snap: &GraphSnapshot) -> Result<(RwLock<Graph>, MemoryStore), LamboError> {
    let graph = graph_of(snap)?;
    let store = MemoryStore::new();
    let mut batch = MutationBatch::new();
    for i in &snap.interactions {
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(i.clone()),
        });
    }
    for c in &snap.concepts {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c.clone()),
        });
    }
    for e in &snap.edges {
        batch.push(Mutation::UpsertEdge { edge: e.clone() });
    }
    store.flush(&batch, None).await?;
    Ok((RwLock::new(graph), store))
}

/// Explain every concept's next hop on `store` at `now`.
async fn survey(
    graph: &RwLock<Graph>,
    store: &dyn GraphStore,
    params: &DryRunParams,
    now: DateTime<Utc>,
) -> Result<Survey, LamboError> {
    let (session, inputs) = {
        let g = graph.read();
        let mut ids: Vec<(NodeId, ConceptType, CanonizationStatus)> = g
            .concepts()
            .map(|c| (c.id, c.concept_type, c.canonization_status))
            .collect();
        ids.sort_by_key(|(id, ..)| id.0);
        let inputs: Vec<_> = ids
            .into_iter()
            .filter_map(|(id, t, s)| {
                admission_inputs(&g, id, &params.eval, now).map(|i| (id, t, s, i))
            })
            .collect();
        (g.session_id().clone(), inputs)
    };
    let mut out = Survey::default();
    let mut one_away = Vec::new();
    let mut load_bearing = Vec::new();
    let mut watched_rows: Vec<IdRow> = Vec::new();
    for (id, t, status, inputs) in inputs {
        let a: Admission = explain_admission(store, &session, &inputs, &params.eval, now).await?;
        let blast = store
            .blast_radius(&session, id, params.eval.min_edge_age, now)
            .await?;
        let hop = hop_label(status, a.next);
        let unmet: Vec<String> = a
            .checks
            .iter()
            .filter(|k| !k.met)
            .map(|k| format!("stage{}/{}", k.stage, k.gate))
            .collect();
        for u in &unmet {
            *out.unmet.entry(format!("{hop} {u}")).or_default() += 1;
        }
        let watched = params.watch.contains(&id);
        let (distinct, coverage) =
            if (blast > 5 && status != CanonizationStatus::Canonical) || watched {
                let span = store
                    .interaction_span_with_window(
                        &session,
                        id,
                        params.eval.min_age,
                        now,
                        params.eval.coverage_window,
                    )
                    .await?;
                (Some(span.distinct), Some(span.coverage))
            } else {
                let d = a.checks.iter().find(|k| k.gate == "distinct_interactions");
                let c = a.checks.iter().find(|k| k.gate == "coverage");
                (d.map(|k| k.current as u64), c.map(|k| k.current))
            };
        let row = IdRow {
            id,
            concept_type: t,
            status,
            next: a.next,
            dependents: blast,
            distinct,
            coverage,
            unmet: unmet.clone(),
        };
        if a.next.is_some() && a.eligible {
            out.eligible.entry(hop.clone()).or_default().add(t);
        }
        // One gate short of the next hop, with some declared dependence (or
        // a Venerable one gate from Canonical): a concept with no
        // dependents at all is not "close", however few gates it fails.
        if a.next.is_some()
            && !a.eligible
            && unmet.len() == 1
            && (blast > 0 || a.next == Some(CanonizationStatus::Canonical))
        {
            out.one_gate_away
                .entry(format!("{hop} {}", unmet[0]))
                .or_default()
                .add(t);
            one_away.push(row.clone());
        }
        // Name the Stage 2 gates that stop it even when its next hop is
        // Stage 1: they decide whether it can ever reach Canonical.
        let with_stage2 = |mut row: IdRow| {
            for (gate, fails) in [
                (
                    "stage2/distinct_interactions",
                    row.distinct.is_some_and(|d| d < 3),
                ),
                ("stage2/coverage", row.coverage.is_some_and(|c| c < 0.3)),
            ] {
                if fails && !row.unmet.iter().any(|u| u == gate) {
                    row.unmet.push(gate.into());
                }
            }
            row
        };
        if watched {
            watched_rows.push(with_stage2(row.clone()));
        }
        if blast > 5 && status != CanonizationStatus::Canonical {
            load_bearing.push(with_stage2(row));
        }
    }
    let by_load = |a: &IdRow, b: &IdRow| b.dependents.cmp(&a.dependents).then(a.id.0.cmp(&b.id.0));
    one_away.sort_by(by_load);
    load_bearing.sort_by(by_load);
    out.load_bearing_total = load_bearing.len();
    one_away.truncate(params.top);
    load_bearing.truncate(params.top);
    out.one_gate_away_top = one_away;
    out.load_bearing = load_bearing;
    out.watched = params
        .watch
        .iter()
        .filter_map(|w| watched_rows.iter().find(|r| r.id == *w).cloned())
        .collect();
    Ok(out)
}

/// How many consecutive empty cycles end the run: at least `min_quiet`, and
/// enough to walk the longest of the Stage 2 and Stage 3 rings once at the
/// batch size, plus three. Empty cycles count only while no re-promotion
/// cooldown is running ([`cooling`]): a node still cooling could promote
/// once it clears, so the run waits that out rather than stop on a
/// countdown.
///
/// The inputs come from the graph and the injected clock, so two runs over
/// one copy stop at the same cycle.
pub fn quiet_needed(g: &Graph, params: &DryRunParams) -> usize {
    let (mut candidates, mut venerables) = (0usize, 0usize);
    for c in g.concepts() {
        match c.canonization_status {
            CanonizationStatus::Candidate => candidates += 1,
            CanonizationStatus::Venerable => venerables += 1,
            _ => {}
        }
    }
    let batch = params.eval.batch_size.max(1);
    let ring = candidates.max(venerables).div_ceil(batch) + 3;
    params.min_quiet_cycles.max(ring)
}

/// Whether any concept is inside its re-promotion cooldown at `now`.
pub fn cooling(g: &Graph, params: &DryRunParams, now: DateTime<Utc>) -> bool {
    g.concepts()
        .any(|c| in_repromotion_cooldown(c.last_demotion_time, params.eval.cooldown, now))
}

/// Every concept's status at a policy's stop.
pub type FinalStatuses = HashMap<NodeId, CanonizationStatus>;

/// One policy's dry run over `snap` (already [`prepare`]d).
pub async fn dry_run(
    snap: &GraphSnapshot,
    params: &DryRunParams,
) -> Result<PolicyReport, LamboError> {
    Ok(run(snap, params).await?.0)
}

/// [`dry_run`], also returning every concept's status at the stop (the
/// alias preview compares two runs concept by concept).
pub async fn run(
    snap: &GraphSnapshot,
    params: &DryRunParams,
) -> Result<(PolicyReport, FinalStatuses), LamboError> {
    let (graph, store) = overlay(snap).await?;
    let start_status: HashMap<NodeId, (CanonizationStatus, ConceptType)> = graph
        .read()
        .concepts()
        .map(|c| (c.id, (c.canonization_status, c.concept_type)))
        .collect();
    let start = census(&graph.read());
    let first_evaluation = survey(&graph, &store, params, params.now).await?;

    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    let mut at = params.now;
    let step = chrono::Duration::from_std(params.step)
        .map_err(|e| LamboError::Config(format!("dry-run step: {e}")))?;
    let (mut cycles, mut idle) = (0usize, 0usize);
    let mut first_cycle = BTreeMap::new();
    let mut hops: BTreeMap<String, usize> = BTreeMap::new();
    let mut reached: BTreeMap<String, Split> = BTreeMap::new();
    let mut reached_ids: HashSet<(String, NodeId)> = HashSet::new();
    let mut illegal: Vec<String> = Vec::new();
    let mut promoted: HashSet<NodeId> = HashSet::new();
    let (stop, quiet) = loop {
        let (quiet, cool) = {
            let g = graph.read();
            (quiet_needed(&g, params), cooling(&g, params, at))
        };
        if cycles >= params.max_cycles {
            break (StopReason::CycleLimit, quiet);
        }
        let scores = {
            let g = graph.read();
            ScoreTable {
                epoch: g.epoch(),
                ranked: rescore(&g, &params.weights),
            }
        };
        let out = ev
            .eval_cycle(&graph, &store, &scores, &tx, &params.eval, at, None)
            .await
            .map_err(LamboError::from)?;
        cycles += 1;
        let mut moved: HashMap<NodeId, usize> = HashMap::new();
        for e in out.transitions() {
            *moved.entry(e.node_id).or_default() += 1;
            if !legal(e) {
                illegal.push(format!(
                    "{} {:?}->{:?}",
                    e.node_id, e.from_status, e.to_status
                ));
            }
            let t = start_status
                .get(&e.node_id)
                .map_or(ConceptType::Resource, |s| s.1);
            *hops
                .entry(format!(
                    "{:?}->{:?} {}",
                    e.from_status,
                    e.to_status,
                    resource_split(t)
                ))
                .or_default() += 1;
            if rank(e.to_status) > rank(e.from_status) {
                promoted.insert(e.node_id);
                let to = status_label(e.to_status);
                if reached_ids.insert((to.clone(), e.node_id)) {
                    reached.entry(to).or_default().add(t);
                }
            }
        }
        illegal.extend(
            moved
                .iter()
                .filter(|(_, n)| **n > 1)
                .map(|(id, n)| format!("{id} moved {n} times in cycle {cycles}")),
        );
        if cycles == 1 {
            first_cycle = hops.clone();
        }
        at += step;
        if out.is_empty() && !cool {
            idle += 1;
            if idle >= quiet {
                break (StopReason::FixedPoint, quiet);
            }
        } else {
            idle = 0;
        }
    };

    let end_at = at;
    let fixed_point = survey(&graph, &store, params, end_at).await?;
    // Read everything the stop needs under one guard, released before the
    // store queries below.
    let (end, demoted, canonical, finals, assert_ok, session) = {
        let g = graph.read();
        let end = census(&g);
        let mut demoted = Split::default();
        let mut canonical = Vec::new();
        for c in g.concepts() {
            if let Some((s, t)) = start_status.get(&c.id)
                && rank(c.canonization_status) < rank(*s)
            {
                demoted.add(*t);
            }
            if c.canonization_status == CanonizationStatus::Canonical {
                canonical.push((c.id, c.concept_type));
            }
        }
        let finals: FinalStatuses = g
            .concepts()
            .map(|c| (c.id, c.canonization_status))
            .collect();
        let assert_ok = g.assert_invariants();
        let session = g.session_id().clone();
        (end, demoted, canonical, finals, assert_ok, session)
    };
    let canonical_total = canonical.len();
    let mut canonical_ids: Vec<NodeId> = canonical.iter().map(|(id, _)| *id).collect();
    canonical_ids.sort_by_key(|id| id.0);
    let budget = params.eval.max_canonical_nodes;

    // Canonicals this run promoted must hold Stage 2 and Stage 3 at the stop.
    let mut rows = Vec::new();
    let mut unsupported = Vec::new();
    for (id, t) in canonical {
        let blast = store
            .blast_radius(&session, id, params.eval.min_edge_age, end_at)
            .await?;
        let span = store
            .interaction_span_with_window(
                &session,
                id,
                params.eval.min_age,
                end_at,
                params.eval.coverage_window,
            )
            .await?;
        if promoted.contains(&id) && (blast <= 5 || !stage2_evidence_passes(span)) {
            unsupported.push(id.to_string());
        }
        rows.push(IdRow {
            id,
            concept_type: t,
            status: CanonizationStatus::Canonical,
            next: None,
            dependents: blast,
            distinct: Some(span.distinct),
            coverage: Some(span.coverage),
            unmet: Vec::new(),
        });
    }
    rows.sort_by(|a, b| b.dependents.cmp(&a.dependents).then(a.id.0.cmp(&b.id.0)));
    rows.truncate(params.top);

    let still_eligible: usize = fixed_point.eligible.values().map(|s| s.total).sum();
    let invariants = vec![
        Invariant::new(
            "fixed_point",
            stop == StopReason::FixedPoint,
            format!("{cycles} cycles, {quiet} quiet cycles needed"),
        ),
        Invariant::new(
            "one_legal_hop_per_cycle",
            illegal.is_empty(),
            illegal.join("; "),
        ),
        Invariant::new(
            "canonical_within_budget",
            canonical_total <= budget,
            format!("{canonical_total} of {budget}"),
        ),
        Invariant::new(
            "promoted_canonicals_hold_stage2_and_stage3",
            unsupported.is_empty(),
            unsupported.join(" "),
        ),
        Invariant::new(
            "nothing_eligible_at_the_stop",
            still_eligible == 0,
            format!("{still_eligible} concepts explained eligible"),
        ),
        Invariant::new(
            "graph_invariants",
            assert_ok.is_ok(),
            assert_ok
                .err()
                .map(|e| redact(&e.to_string()))
                .unwrap_or_default(),
        ),
    ];

    let report = PolicyReport {
        policy: params.eval.promotion_policy,
        start,
        first_evaluation,
        cycles,
        stop,
        quiet_needed: quiet,
        budget_saturated: canonical_total >= budget,
        first_cycle,
        hops,
        reached,
        demoted,
        end,
        fixed_point,
        canonical: rows,
        canonical_total,
        canonical_ids,
        invariants,
    };
    Ok((report, finals))
}

/// Whether `e` is one legal rung of the evaluator's ladder (the eval loop
/// never takes `None -> Venerable`; a demotion is `Canonical -> None`).
fn legal(e: &CanonizationEvent) -> bool {
    use CanonizationStatus::{Candidate, Canonical, Venerable};
    matches!(
        (e.from_status, e.to_status),
        (CanonizationStatus::None, Candidate)
            | (Candidate, Venerable)
            | (Venerable, Canonical)
            | (Canonical, CanonizationStatus::None)
    )
}

/// Run every policy in `policies` over `snap` and assemble the report.
pub async fn dry_run_all(
    mut snap: GraphSnapshot,
    policies: &[PromotionPolicy],
    base: &DryRunParams,
) -> Result<DryRunReport, LamboError> {
    let raised = prepare(&mut snap, base.assume_survived);
    let (clock, rows_after_clock) = clock_check(&snap, base.now);
    let mut reports = Vec::new();
    for &policy in policies {
        let mut params = base.clone();
        params.eval.promotion_policy = policy;
        reports.push(dry_run(&snap, &params).await?);
    }
    Ok(DryRunReport {
        version: REPORT_VERSION,
        session: snap.session_id.clone(),
        at: base.now,
        concepts: snap.concepts.len(),
        interactions: snap.interactions.len(),
        edges: snap.edges.len(),
        survival_raised: raised,
        supersede: base.eval.supersede,
        rows_after_clock,
        source: None,
        policies: reports,
        invariants: vec![clock],
        comparison: None,
    })
}

/// Rows dated after the evaluation clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowsAfterClock {
    /// Interactions written or dated after it.
    pub interactions: usize,
    /// Edges written or dated after it.
    pub edges: usize,
    /// Concepts demoted after it.
    pub demotions: usize,
}

impl RowsAfterClock {
    /// All of them.
    pub fn total(&self) -> usize {
        self.interactions + self.edges + self.demotions
    }
}

/// Review M1: the clock must not be earlier than the newest row. The stores
/// age every edge and interaction against the clock, so a row dated after it
/// is not "young", it is silently left out, and a clock before most of the
/// data reports a run on a fraction of the session. A failure is an
/// invariant failure, not a usage refusal: the clock is well-formed, the
/// report still shows what was measured, and the command exits 1 so the
/// result cannot pass for a real dry run.
pub fn clock_check(snap: &GraphSnapshot, at: DateTime<Utc>) -> (Invariant, RowsAfterClock) {
    let later = |a: DateTime<Utc>, b: Option<DateTime<Utc>>| a > at || b.is_some_and(|b| b > at);
    let rows = RowsAfterClock {
        interactions: snap
            .interactions
            .iter()
            .filter(|i| later(i.created_at, i.event_time))
            .count(),
        edges: snap
            .edges
            .iter()
            .filter(|e| later(e.created_at, e.event_time))
            .count(),
        demotions: snap
            .concepts
            .iter()
            .filter(|c| c.last_demotion_time.is_some_and(|t| t > at))
            .count(),
    };
    let held = rows.total() == 0;
    let detail = if held {
        String::new()
    } else {
        format!(
            "--at {} is earlier than the data: {} interactions, {} edges and {} demotions are dated after it and would be left out; pin --at at or after the newest row (the backup's time), or omit it",
            at.to_rfc3339(),
            rows.interactions,
            rows.edges,
            rows.demotions
        )
    };
    (Invariant::new("clock_after_the_data", held, detail), rows)
}

/// `snap` as a graph, with any invariant error redacted ([`redact`]).
pub(crate) fn graph_of(snap: &GraphSnapshot) -> Result<Graph, LamboError> {
    Graph::from_snapshot(snap.clone()).map_err(|e| {
        LamboError::Config(format!(
            "the session copy fails the graph's invariants: {}",
            redact(&e.to_string())
        ))
    })
}

/// Keep only the ids in a store or graph error (review L3): an invariant
/// message can quote a canonical key, which is derived from concept text.
pub fn redact(message: &str) -> String {
    let ids: Vec<&str> = message
        .split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .filter(|w| crate::surface::resolve::is_hyphenated_id(w))
        .collect();
    if ids.is_empty() {
        "detail withheld (it may quote concept text)".into()
    } else {
        format!(
            "detail withheld (it may quote concept text); ids involved: {}",
            ids.join(" ")
        )
    }
}

#[cfg(all(test, feature = "fixtures"))]
#[path = "dry_run_tests.rs"]
mod tests;
