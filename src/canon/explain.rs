//! Why a concept is or is not admitted to its next canonization status
//! (#36 PR 3) — the explanation `lambo_inspect`, `lambo inspect` and the
//! portal's `/api/inspect` show per node.
//!
//! The explanation runs the evaluator's own predicates on the evaluator's own
//! inputs, never a parallel metric:
//!
//! * Stage 1 reads the concept's `gc_survived` and
//!   [`dependent_interactions`](crate::canon::dependent_interactions), the
//!   single-node form of the batched count the cycle uses, with the cycle's
//!   age floor; Solo reads its [`SoloBreakdown`].
//! * Stage 2 is [`GraphStore::interaction_span_with_window`] with the cycle's
//!   age floor and coverage cap, compared by [`stage2_evidence_passes`]'s bars.
//! * Stage 3 is Stage 2 again plus [`GraphStore::blast_radius`] with the
//!   cycle's edge age floor, and the re-promotion cooldown, both policies.
//!
//! It answers one question: **would the next hop's gates pass if the node
//! were evaluated now?** Whether a cycle reaches it this tick is a matter of
//! the batch, the ring cursors and the Canonical budget, which the
//! explanation names but does not predict.
//!
//! A store without the window-aware span is explained the way the evaluator
//! treats it: Stage 2 has no evidence, and the explanation says why.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::canon::policy::{
    solo_breakdown, SoloBreakdown, CANDIDATE_BAR, CANONICAL_BAR, VENERABLE_BAR,
};
use crate::canon::stage1::{dependent_interactions, MIN_DEPENDENT_INTERACTIONS, MIN_GC_SURVIVED};
use crate::canon::stage2::{MIN_COVERAGE, MIN_DISTINCT};
use crate::canon::stage3::{in_repromotion_cooldown, MIN_BLAST_RADIUS};
use crate::canon::{EvalParams, PromotionPolicy};
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::InteractionSpan;
use crate::types::{CanonizationStatus, Concept, Node, NodeId, SessionId, StoreError};

/// How a check compares its measurement with its bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// `current >= bar`.
    AtLeast,
    /// `current > bar` (blast radius).
    Above,
    /// `current <= bar` (seconds of cooldown left, bar 0).
    AtMost,
    /// `current < bar` (Canonical count against the budget).
    Below,
}

/// One gate of the next hop: what it measures, the concept's value, the bar,
/// and whether it holds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AdmissionCheck {
    /// Stable machine name: `gc_survived`, `dependent_interactions`,
    /// `distinct_interactions`, `coverage`, `blast_radius`, `cooldown`,
    /// `canonical_budget`, `solo_score`, `held` (#20: supersession would
    /// demote it at once).
    pub gate: &'static str,
    /// The stage whose verdict this check is part of (1, 2 or 3).
    pub stage: u8,
    /// The concept's measurement.
    pub current: f64,
    /// The bar it is compared with.
    pub bar: f64,
    /// How the two compare.
    pub comparison: Comparison,
    /// Whether it holds.
    pub met: bool,
}

impl AdmissionCheck {
    fn new(gate: &'static str, stage: u8, current: f64, bar: f64, comparison: Comparison) -> Self {
        let met = current.is_finite()
            && match comparison {
                Comparison::AtLeast => current >= bar,
                Comparison::Above => current > bar,
                Comparison::AtMost => current <= bar,
                Comparison::Below => current < bar,
            };
        Self {
            gate,
            stage,
            current,
            bar,
            comparison,
            met,
        }
    }

    /// One short sentence for an unmet check.
    fn reason(&self) -> String {
        let n = |v: f64| {
            if v.fract() == 0.0 {
                format!("{v:.0}")
            } else {
                format!("{v:.2}")
            }
        };
        match self.gate {
            "gc_survived" => format!(
                "survived {} GC sweeps, needs at least {}",
                n(self.current),
                n(self.bar)
            ),
            "dependent_interactions" => format!(
                "dependents from {} other interactions, needs at least {}",
                n(self.current),
                n(self.bar)
            ),
            "distinct_interactions" => format!(
                "stage 2: dependents from {} other interactions, needs at least {}",
                n(self.current),
                n(self.bar)
            ),
            "coverage" => format!(
                "stage 2: coverage {:.2}, needs at least {:.2}",
                self.current, self.bar
            ),
            "blast_radius" => format!(
                "stage 3: {} dependents, needs more than {}",
                n(self.current),
                n(self.bar)
            ),
            "cooldown" => format!(
                "stage 3: demoted recently, {}s of the re-promotion cooldown left",
                n(self.current.ceil())
            ),
            "canonical_budget" => format!(
                "stage 3: the Canonical budget is full ({} of {}), so no Stage 3 \
                 window runs until a Canonical is demoted",
                n(self.current),
                n(self.bar)
            ),
            "solo_score" => format!(
                "solo score {:.2}, needs at least {:.2}",
                self.current, self.bar
            ),
            other => format!("{other}: {} against {}", n(self.current), n(self.bar)),
        }
    }
}

/// Why a concept is or is not admitted to its next status under the live
/// policy. Additive on every wire it appears on.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Admission {
    /// The policy whose gates this describes.
    pub policy: PromotionPolicy,
    /// The concept's current status.
    pub status: CanonizationStatus,
    /// The next status up the ladder; absent for a Canonical concept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<CanonizationStatus>,
    /// Whether every gate of the next hop holds now.
    pub eligible: bool,
    /// The next hop's gates, in evaluation order.
    pub checks: Vec<AdmissionCheck>,
    /// Solo's score terms, under Solo only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solo: Option<SoloBreakdown>,
    /// One sentence per unmet gate, or one sentence saying what still
    /// decides the timing when every gate holds.
    pub reasons: Vec<String>,
}

/// The graph-side inputs of an [`Admission`], read under the caller's read
/// guard so the store queries can run with no lock held.
#[derive(Clone, Debug)]
pub struct AdmissionInputs {
    concept: Concept,
    dependent_interactions: usize,
    /// Solo's score terms; computed only when the inputs were read under
    /// Solo (#36 PR 3 review L2).
    solo: Option<SoloBreakdown>,
    /// `(canonical count, budget)` when the next hop is Canonical: Stage 3
    /// evaluates nothing while the budget is full.
    budget: Option<(usize, usize)>,
    /// #20 review M2: the evaluator's hold probes for a Venerable concept
    /// when supersession is on (empty otherwise).
    hold: crate::canon::supersession::Probes,
}

/// Read `node`'s graph-side inputs under `params`' policy, or `None` when it
/// is not a concept. Pass the same `params` to [`explain_admission`].
pub fn admission_inputs(
    graph: &Graph,
    node: NodeId,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Option<AdmissionInputs> {
    let Some(Node::Concept(c)) = graph.node(node) else {
        return None;
    };
    let deps = dependent_interactions(graph, node, params.min_age, now);
    let solo = match params.promotion_policy {
        PromotionPolicy::Solo => Some(solo_breakdown(graph, c, deps)),
        PromotionPolicy::Swarm => None,
    };
    let budget = (c.canonization_status == CanonizationStatus::Venerable).then(|| {
        let canonical = graph
            .concepts()
            .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
            .count();
        (canonical, params.max_canonical_nodes)
    });
    Some(AdmissionInputs {
        concept: c.clone(),
        dependent_interactions: deps,
        solo,
        budget,
        hold: crate::canon::supersession::gather_hold_for(graph, node, params, now),
    })
}

impl AdmissionInputs {
    /// Which store measurements the next hop needs: `(span, blast)`.
    /// Candidate → Venerable needs the span; Venerable → Canonical both.
    pub fn needs(&self) -> (bool, bool) {
        match self.concept.canonization_status {
            CanonizationStatus::Candidate => (true, false),
            CanonizationStatus::Venerable => (true, true),
            CanonizationStatus::None | CanonizationStatus::Canonical => (false, false),
        }
    }
}

/// The store measurements Stages 2 and 3 compare, taken once so callers that
/// need them twice (the portal's `gate_progress` and `admission`, review L2)
/// share one round trip each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurements {
    /// Stage 2's capped span; `Err(capability)` when the store does not
    /// implement `interaction_span_with_window`. `None` when not measured.
    pub span: Option<Result<InteractionSpan, String>>,
    /// Stage 3's aged dependent count. `None` when not measured.
    pub blast: Option<u64>,
    /// #20 review M2: the contradicting concept that holds this Venerable at
    /// Venerable (supersession on, its newer evidence would supersede it at
    /// once, under the pair rule); `None` when nothing holds it or it was not
    /// measured ([`Self::take_hold`]).
    pub held_by: Option<NodeId>,
}

impl Measurements {
    /// Measure `node` with the evaluator's own queries and parameters: the
    /// span when `span`, the blast radius when `blast`. A missing
    /// window-aware span is recorded, not raised; any other store error is.
    pub async fn take(
        store: &dyn GraphStore,
        session: &SessionId,
        node: NodeId,
        params: &EvalParams,
        now: DateTime<Utc>,
        span: bool,
        blast: bool,
    ) -> Result<Self, StoreError> {
        let span = if span {
            match store
                .interaction_span_with_window(
                    session,
                    node,
                    params.min_age,
                    now,
                    params.coverage_window,
                )
                .await
            {
                Ok(s) => Some(Ok(s)),
                Err(StoreError::Capability(what)) => Some(Err(what)),
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let blast = if blast {
            Some(
                store
                    .blast_radius(session, node, params.min_edge_age, now)
                    .await?,
            )
        } else {
            None
        };
        Ok(Self {
            span,
            blast,
            held_by: None,
        })
    }

    /// Run the evaluator's hold probe for `inputs` (#20 review M2): the same
    /// probes, the same store reads and the same pair rule as a cycle.
    /// Nothing to do unless supersession is on and the concept is Venerable;
    /// a store without `dependent_evidence` holds nothing, as in a cycle.
    pub async fn take_hold(
        &mut self,
        store: &dyn GraphStore,
        session: &SessionId,
        inputs: &AdmissionInputs,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if inputs.hold.is_empty() {
            return Ok(());
        }
        let mut cache = crate::canon::supersession::EvidenceCache::default();
        match crate::canon::supersession::decide(
            store,
            session,
            &inputs.hold,
            params,
            now,
            &mut cache,
        )
        .await
        {
            Ok(v) => {
                self.held_by = v
                    .into_iter()
                    .find(|v| v.target == inputs.concept.id)
                    .map(|v| v.source);
                Ok(())
            }
            Err(StoreError::Capability(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Explain the next hop of the concept `inputs` was read for. Issues the
/// Stage 2/3 store queries only when the next hop needs them.
pub async fn explain_admission(
    store: &dyn GraphStore,
    session: &SessionId,
    inputs: &AdmissionInputs,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<Admission, StoreError> {
    let (span, blast) = inputs.needs();
    let mut m =
        Measurements::take(store, session, inputs.concept.id, params, now, span, blast).await?;
    m.take_hold(store, session, inputs, params, now).await?;
    Ok(explain_measured(inputs, params, now, &m))
}

/// [`explain_admission`] on measurements already taken (at least those
/// [`AdmissionInputs::needs`] names). A needed measurement that is missing
/// is reported as unavailable, never as a pass.
pub fn explain_measured(
    inputs: &AdmissionInputs,
    params: &EvalParams,
    now: DateTime<Utc>,
    m: &Measurements,
) -> Admission {
    let c = &inputs.concept;
    let policy = params.promotion_policy;
    let solo = match policy {
        PromotionPolicy::Solo => inputs.solo,
        PromotionPolicy::Swarm => None,
    };
    // Inputs read under another policy carry no Solo terms: the score check
    // then reads NaN and fails, rather than inventing a score.
    let solo_score = inputs.solo.map_or(f64::NAN, |b| b.score);
    let next = match c.canonization_status {
        CanonizationStatus::None => Some(CanonizationStatus::Candidate),
        CanonizationStatus::Candidate => Some(CanonizationStatus::Venerable),
        CanonizationStatus::Venerable => Some(CanonizationStatus::Canonical),
        CanonizationStatus::Canonical => None,
    };
    let mut checks = Vec::new();
    let mut unavailable: Option<String> = None;
    match next {
        None => {}
        Some(CanonizationStatus::Candidate) => match policy {
            PromotionPolicy::Swarm => {
                checks.push(AdmissionCheck::new(
                    "gc_survived",
                    1,
                    c.gc_survived as f64,
                    MIN_GC_SURVIVED as f64,
                    Comparison::AtLeast,
                ));
                checks.push(AdmissionCheck::new(
                    "dependent_interactions",
                    1,
                    inputs.dependent_interactions as f64,
                    MIN_DEPENDENT_INTERACTIONS as f64,
                    Comparison::AtLeast,
                ));
            }
            PromotionPolicy::Solo => {
                checks.push(solo_check(1, solo_score, CANDIDATE_BAR));
            }
        },
        Some(to) => {
            let stage = if to == CanonizationStatus::Venerable {
                2
            } else {
                3
            };
            if policy == PromotionPolicy::Solo {
                let bar = if stage == 2 {
                    VENERABLE_BAR
                } else {
                    CANONICAL_BAR
                };
                checks.push(solo_check(stage, solo_score, bar));
            }
            match &m.span {
                Some(Ok(span)) => {
                    checks.push(AdmissionCheck::new(
                        "distinct_interactions",
                        2,
                        span.distinct as f64,
                        MIN_DISTINCT as f64,
                        Comparison::AtLeast,
                    ));
                    checks.push(AdmissionCheck::new(
                        "coverage",
                        2,
                        span.coverage,
                        MIN_COVERAGE,
                        Comparison::AtLeast,
                    ));
                }
                Some(Err(what)) => unavailable = Some(what.clone()),
                None => unavailable = Some("the Stage 2 span (not measured)".into()),
            }
            if stage == 3 {
                checks.push(AdmissionCheck::new(
                    "blast_radius",
                    3,
                    m.blast.map_or(f64::NAN, |b| b as f64),
                    MIN_BLAST_RADIUS as f64,
                    Comparison::Above,
                ));
                checks.push(AdmissionCheck::new(
                    "cooldown",
                    3,
                    cooldown_left(c.last_demotion_time, params.cooldown, now),
                    0.0,
                    Comparison::AtMost,
                ));
                // Review L9: with the budget full the cycle evaluates no
                // Stage 3 window at all, so the hop cannot happen.
                // #20 review M2: the evaluator's hold, as a failing check.
                if m.held_by.is_some() {
                    checks.push(AdmissionCheck::new("held", 3, 1.0, 0.0, Comparison::AtMost));
                }
                if let Some((canonical, budget)) = inputs.budget {
                    checks.push(AdmissionCheck::new(
                        "canonical_budget",
                        3,
                        canonical as f64,
                        budget as f64,
                        Comparison::Below,
                    ));
                }
            }
        }
    }
    let eligible = next.is_some() && unavailable.is_none() && checks.iter().all(|k| k.met);
    let mut reasons: Vec<String> = Vec::new();
    if let Some(what) = &unavailable {
        reasons.push(format!(
            "stage 2: the store cannot measure {what}, so there is no evidence"
        ));
    }
    reasons.extend(
        checks
            .iter()
            .filter(|k| !k.met)
            .map(|k| match (k.gate, m.held_by) {
                ("held", Some(by)) => format!(
                    "held: contradicted by {} with newer evidence",
                    crate::recall::format::short_id(by)
                ),
                _ => k.reason(),
            }),
    );
    match next {
        None => reasons.push("canonical: no further promotion".into()),
        Some(_) if eligible => {
            reasons.push("every gate holds; the cycle's batch and ring order decide when".into())
        }
        Some(_) => {}
    }
    Admission {
        policy,
        status: c.canonization_status,
        next,
        eligible,
        checks,
        solo,
        reasons,
    }
}

fn solo_check(stage: u8, score: f64, bar: f64) -> AdmissionCheck {
    AdmissionCheck::new("solo_score", stage, score, bar, Comparison::AtLeast)
}

/// Seconds of re-promotion cooldown left, `0.0` when clear. Mirrors
/// `stage3::in_repromotion_cooldown`, including its conservative answer for
/// an unrepresentable cooldown (still cooling: infinite).
fn cooldown_left(last: Option<DateTime<Utc>>, cooldown: Duration, now: DateTime<Utc>) -> f64 {
    if !in_repromotion_cooldown(last, cooldown, now) {
        return 0.0;
    }
    let until = last.and_then(|t| {
        chrono::Duration::from_std(cooldown)
            .ok()
            .and_then(|d| t.checked_add_signed(d))
    });
    match until {
        Some(until) => ((until - now).num_milliseconds() as f64 / 1000.0).max(f64::MIN_POSITIVE),
        None => f64::INFINITY,
    }
}

/// The explanation as inspect text: a header line and one indented row per
/// reason.
///
/// ```text
/// canonization (Swarm): Candidate -> Venerable refused
///   - stage 2: dependents from 2 other interactions, needs at least 3
/// ```
pub fn render_admission(a: &Admission) -> String {
    let head = match a.next {
        None => format!("canonization ({}): {:?}", a.policy, a.status),
        Some(next) => format!(
            "canonization ({}): {:?} -> {:?} {}",
            a.policy,
            a.status,
            next,
            if a.eligible { "eligible" } else { "refused" }
        ),
    };
    let mut out = head;
    out.push('\n');
    for r in &a.reasons {
        out.push_str("  - ");
        out.push_str(r);
        out.push('\n');
    }
    out
}

/// Inspect's line for `node`'s latest demotion (#20), when it has one: why
/// (`budget`, or `superseded by` the contradicting concept, quoted while it
/// is in the graph and named by id either way) and when.
///
/// ```text
/// last demotion 2026-09-30T12:00:00Z: superseded by "flushes give up after three attempts" (id 1a2b3c4d)
/// last demotion 2026-09-30T12:00:00Z: budget
/// ```
pub(crate) fn render_last_demotion(graph: &Graph, node: NodeId) -> Option<String> {
    let event = graph
        .canonization_events()
        .iter()
        .rev()
        .find(|e| e.node_id == node && e.is_demotion())?;
    let at = event
        .occurred_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let why = match event.demotion_reason()? {
        crate::types::DemotionReason::Budget => "budget".to_string(),
        crate::types::DemotionReason::Superseded { by } => {
            let id = crate::recall::format::short_id(by);
            match graph.node(by) {
                // Review L6: the same 80-character excerpt recall quotes.
                Some(crate::types::Node::Concept(c)) => format!(
                    "superseded by \"{}\" (id {id})",
                    crate::recall::format::excerpt(&c.content)
                ),
                _ => format!("superseded by a concept no longer in memory (id {id})"),
            }
        }
        crate::types::DemotionReason::Unknown {
            reason,
            superseded_by,
        } => format!(
            "a reason this build cannot read (reason {}, superseded_by {})",
            reason.as_deref().unwrap_or("none"),
            superseded_by.map_or("none".to_string(), crate::recall::format::short_id)
        ),
    };
    Some(format!("last demotion {at}: {why}\n"))
}

/// The canonization section inspect prints: the admission explanation, then
/// the latest demotion line (#20).
pub(crate) fn with_last_demotion(
    section: Option<String>,
    graph: &Graph,
    node: NodeId,
) -> Option<String> {
    match (section, render_last_demotion(graph, node)) {
        (Some(s), Some(d)) => Some(s + &d),
        (s, d) => s.or(d),
    }
}

/// Inspect's explanation for `node`: [`admission_inputs`] under a short read
/// of `graph`, then [`explain_admission`] with no lock held. `None` when
/// `node` is not a concept.
pub(crate) async fn explain_node(
    graph: &parking_lot::RwLock<Graph>,
    store: &dyn GraphStore,
    node: NodeId,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<Option<Admission>, StoreError> {
    let (session, inputs) = {
        let g = graph.read();
        (
            g.session_id().clone(),
            admission_inputs(&g, node, params, now),
        )
    };
    match inputs {
        Some(inputs) => Ok(Some(
            explain_admission(store, &session, &inputs, params, now).await?,
        )),
        None => Ok(None),
    }
}
