//! Promotion policy selection — the `PromotionScorer` seam (C1).
//!
//! Each policy owns two decisions: **Stage 1 admission** (which concepts
//! become Candidates) and the **admission of the later hops** given a stage's
//! evidence verdict ([`PromotionScorer::admits_hop`]). Stages 2 and 3
//! *measure* evidence (`interaction_span_with_window`, `blast_radius`), and
//! those measurements are policy-independent: evidence means the same thing
//! whether one writer or twenty produced it.
//!
//! ## Since #36 PR 3: evidence first, policy second
//!
//! * **Swarm** admits a Candidate on evidence alone: `gc_survived >= 3` and
//!   dependents from at least two interactions other than the concept's own
//!   ([`crate::canon::stage1_candidates`]). The old 20-peer minimum and the
//!   P90 quota are gone; the composite score only orders the queue. Each later
//!   hop is its stage's verdict.
//! * **Solo** admits a Candidate by its score band (spec §3.2's formula, with
//!   the valid-action term replaced by distinct dependent interactions), and
//!   each later hop needs **both** its band and its stage's verdict. Solo can
//!   no longer climb past Stage 2, Stage 3 or the re-promotion cooldown on its
//!   score: the evaluator requires the verdict before it asks the policy at
//!   all, so a policy can only narrow the evidence, never replace it.
//!
//! Why: on the rig snapshot, Solo by bands alone canonized 121 file paths and
//! short names with blast radius 0 (100% artifacts), ranked by how often
//! actions touched them; the valid-action term counted `Causal` edges, which
//! #36 decided confer no load. Evidence-gated Solo matched Swarm's set in the
//! design measurement. See `dev-diary/notes/feature-36-pr3-admission.md`.
//!
//! ## The solo score
//!
//! ```text
//! (Sessions × 1.0) + (Human Confirmed × 4.0)
//!     + (Dependent Interactions × 2.0) − (Reverts × 3.0)
//! ```
//!
//! multiplied by the concept-type eviction-resistance table
//! ([`crate::types::ConceptType::eviction_resistance`]) before the band
//! comparison: ≥ [`CANONICAL_BAR`] Canonical, ≥ [`VENERABLE_BAR`] Venerable,
//! ≥ [`CANDIDATE_BAR`] Candidate, else None. `Dependent Interactions` is the
//! same aged, own-origin-excluded count Swarm's Stage 1 reads
//! ([`crate::canon::dependent_interaction_counts`]), so one write declaring a
//! dependence moves both policies the same way, and `Causal` touches (a file
//! modified by many actions) no longer score. `human_confirmed` is a
//! persisted counter bumped only through [`crate::Memory::confirm_human`].
//!
//! The recurrence term is the reason C2 depends on D2: "sessions" are counted
//! with [`separated_session_count`] over the **event-time** spread of the
//! concept's supporting interactions ([`crate::types::Interaction::about_time`],
//! never flush stamps). A bulk ingest of a decade of history in ninety
//! minutes has that separation in commit dates and none whatsoever in flush
//! time. The test that pins the difference is
//! `a_bulk_ingest_recurs_only_under_event_time` below.
//!
//! ## Shape: enum on the params, trait for the behaviour
//!
//! The selector rides on [`crate::canon::EvalParams`], which is already "the
//! knobs for one cycle". Nothing here duplicates one of its thresholds: the
//! policy chooses *which predicate reads them*, it does not restate one.
//!
//! ## Injected clock
//!
//! [`PromotionScorer::candidates`] and [`PromotionScorer::admits_hop`] take
//! `now`, used for the dependent-interaction age floor; nothing here reaches
//! for `Utc::now()`.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::canon::event_time::separated_session_count;
use crate::canon::stage1::{dependent_interaction_counts, dependent_interactions};
use crate::canon::stage1_candidates;
use crate::canon::EvalParams;
use crate::graph::Graph;
use crate::types::{
    tie_break_by_key, CanonizationStatus, Concept, EdgeType, Node, NodeId, ScoreTable,
};

/// Declares [`PromotionPolicy`] **and** [`PromotionPolicy::ALL`] from one list
/// of variants, so that a variant the valid set does not name is not a thing
/// this file can express.
///
/// # Why the enum is not written out by hand (T3-P3-2)
///
/// `ALL` is what every refusal enumerates and what [`PromotionPolicy::from_str`]
/// searches, so a variant missing from it is not cosmetic. It is unselectable
/// from *both* operator surfaces — `lambo.toml`'s `promotion_policy` and
/// `LAMBO_PROMOTION_POLICY` — while every refusal keeps naming only the values
/// that are in the array, so the operator is told the value they need does not
/// exist.
///
/// That property used to be "pinned by construction" with `ALL.len() == 2` plus
/// a round-trip loop over `ALL`. Both are blind in the one direction that
/// matters: the length only moves when a variant is added *to `ALL`*, and the
/// loop iterates `ALL`, so a variant absent from `ALL` was reached by neither.
/// A third variant, with the arms the compiler demands in `scorer`, `as_str`
/// and `gate_progress`, compiled and passed the entire suite while being
/// unreachable from either surface.
///
/// An exhaustive `match` does not close it either, and the reason is worth
/// recording so the next author does not retry it: every match-based scheme
/// needs a second, independent statement of *how many* variants exist to bound
/// the arms against, and stable Rust cannot count an enum's variants. So the
/// arm the compiler forces for the new variant can always be satisfied —
/// pointing at an existing slot, or returning the same index as its neighbour —
/// without `ALL` growing. Generating both from one list is what actually
/// closes it: there is no way to add a variant except by adding it here, and
/// adding it here adds it to `ALL`. What the compiler then still does, and does
/// well, is point at every match that has to gain an arm.
macro_rules! promotion_policy {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
        #[serde(rename_all = "PascalCase")]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )+
        }

        impl $name {
            /// Every value an operator may write, in the spelling config
            /// serializes.
            ///
            /// The valid set exists once so that no error message can drift
            /// from what [`PromotionPolicy::from_str`] actually accepts: a
            /// message naming `Swarm, Solo` from a hardcoded literal keeps
            /// saying that after a third variant lands, and an operator then
            /// reads a refusal that omits the very value they should have used.
            ///
            /// Generated from the same variant list as the enum itself (see the
            /// `promotion_policy!` macro), including the length — so this is
            /// exhaustive by construction rather than by assertion, and there
            /// is no literal count for the two to disagree about.
            pub const ALL: [$name; [$(stringify!($variant)),+].len()] =
                [$($name::$variant),+];
        }
    };
}

promotion_policy! {
    /// Which promotion policy a session canonizes under.
    ///
    /// Serialized `PascalCase`, matching [`crate::types::MatchStrategy`] — the
    /// other enum-valued knob on [`crate::Config`].
    ///
    /// Unlike `MatchStrategy`, the `Default` here and
    /// `Config::default().promotion_policy` are the **same** value (`Swarm`).
    /// There is deliberately no second, differing product default to remember:
    /// swarm is what the pipeline has always done, and C1's entire contract is
    /// that it keeps doing it.
    ///
    /// Declared through the `promotion_policy!` macro so that this variant list
    /// is also [`PromotionPolicy::ALL`]. **A new variant goes here**, and the
    /// compiler will then name every `match` that needs an arm for it.
    pub enum PromotionPolicy {
        /// Evidence-led admission (#36 PR 3): a Candidate needs `gc_survived >= 3`
        /// and dependents from at least two other interactions; each later hop is
        /// its stage's verdict. The default.
        #[default]
        Swarm,
        /// Single-writer promotion (spec §3.2's solo score): Stage 1 by score
        /// band, each later hop by band **and** its stage's verdict (#36 PR 3).
        /// Nothing canonizes under solo unless a session opts in.
        Solo,
    }
}

impl PromotionPolicy {
    /// The accepted values as one `Swarm | Solo` phrase, built from
    /// [`PromotionPolicy::ALL`] — the same shape `StoreKind` /
    /// `EmbedderKind` put in their own refusals.
    fn expected() -> String {
        Self::ALL
            .iter()
            .map(|policy| policy.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// The scorer this policy dispatches to.
    ///
    /// Both scorers are zero-sized, so this is a `&'static` with no
    /// allocation and no lifetime plumbing at the call site.
    pub fn scorer(self) -> &'static dyn PromotionScorer {
        match self {
            PromotionPolicy::Swarm => &SwarmScorer,
            PromotionPolicy::Solo => &SoloScorer,
        }
    }

    /// The value as it is written in config — for error messages, for the
    /// attach line, and for `lambo_stats`.
    ///
    /// The runtime report matters as much as the refusal. `lambo.toml` saying
    /// `Solo` while a stale `LAMBO_PROMOTION_POLICY=Swarm` is exported in a
    /// systemd unit is a *valid* configuration whose env half wins silently;
    /// without this value on the attach line and in the stats payload the only
    /// remaining evidence of which policy is live is days of absent
    /// canonization events, which is exactly the diagnosis dead-end the
    /// selector exists to end.
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionPolicy::Swarm => "Swarm",
            PromotionPolicy::Solo => "Solo",
        }
    }
}

/// Parse a config or environment spelling: trimmed, case-insensitive, and
/// refusing anything else by name.
///
/// Deliberately as lenient as its two siblings, `StoreKind::from_str` and
/// `EmbedderKind::from_str`, which both trim and lowercase. `lambo.toml`'s
/// every other value is snake_case (`kind = "memory"`, `kind = "bge_m3"`), so
/// `promotion_policy = "solo"` is the likeliest thing an operator types; a
/// parser that refused it would be teaching a casing rule that exists nowhere
/// else in the file. The refusal still names the rejected value *and* the valid
/// set, because the fail-closed contract is about never silently falling back
/// to `Swarm`, not about casing.
///
/// One parser serves both surfaces: the file goes through it via
/// `LamboFile`'s `deserialize_with`, and the environment overlay calls it
/// directly. An env-lenient/file-strict split would mean the same string
/// works in `LAMBO_PROMOTION_POLICY` and fails in `lambo.toml`.
impl FromStr for PromotionPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(format!(
                "empty promotion policy (expected {})",
                Self::expected()
            ));
        }
        let lowered = t.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|policy| policy.as_str().to_ascii_lowercase() == lowered)
            .ok_or_else(|| {
                // A short word is quoted back so the typo is visible; anything
                // else (a DSN or token pasted under the wrong key) is not,
                // because the message reaches the startup log.
                let shown = if t.len() <= 24
                    && t.chars()
                        .all(|c| c.is_ascii_alphanumeric() || " _-".contains(c))
                {
                    format!("{t:?}")
                } else {
                    "(value not shown)".to_string()
                };
                format!(
                    "unknown promotion policy {shown} (expected {})",
                    Self::expected()
                )
            })
    }
}

impl std::fmt::Display for PromotionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `serde` entry point for `lambo.toml`'s optional `promotion_policy` key, so
/// the file is parsed by [`PromotionPolicy::from_str`] rather than by the
/// derive's exact-`PascalCase` match.
///
/// Only the *process file* is routed here. `Config`'s own field keeps the
/// derive, because `Config` is a Rust struct an embedder builds in code and
/// its JSON is a library wire format, not something an operator hand-types.
pub(crate) fn deserialize_config_value<'de, D>(de: D) -> Result<Option<PromotionPolicy>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<String>::deserialize(de)? {
        None => Ok(None),
        Some(raw) => raw
            .parse::<PromotionPolicy>()
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// The Stage-1 promotion decision and each later hop's admission, as a policy.
///
/// Stages 2 and 3 ask the store for evidence and their predicates are
/// policy-independent (see the module docs). What the policy owns is Stage 1
/// and the **admission** of the Candidate → Venerable and Venerable →
/// Canonical hops: [`PromotionScorer::admits_hop`] receives the stage's
/// evidence verdict and may refuse a hop the evidence supports. Since #36
/// PR 3 it can no longer admit a hop the evidence refuses: the evaluator
/// asks the policy only about a node whose stage verdict passed, so the
/// policy can narrow the evidence but never replace it. (The method lost its
/// `evidence` argument in the same release: it was always `true`.)
pub trait PromotionScorer: Send + Sync + std::fmt::Debug {
    /// Concepts that currently clear the Candidate bar, in the order a
    /// bounded cycle should take them (highest priority first).
    ///
    /// `now` is injected rather than read from the clock, with
    /// `params.min_age` the dependence age floor.
    fn candidates(
        &self,
        graph: &Graph,
        scores: &ScoreTable,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Vec<NodeId>;

    /// Whether the policy lets `node` take the promotion hop to `to`, given
    /// that the stage verdict for that hop holds (Stage 2's capped span, or
    /// Stage 3's "Stage 2 still holds, blast radius > 5, cooldown clear"):
    /// the evaluator never asks about a node whose verdict failed. It also
    /// asks before probing (`Evaluator::gather`), so a node the policy would
    /// refuse costs no store query. A `to` below the node's current status is
    /// never asked — the ladder only climbs.
    fn admits_hop(
        &self,
        graph: &Graph,
        node: NodeId,
        to: CanonizationStatus,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> bool;
}

/// Evidence-led Swarm admission — the default policy.
///
/// Stage 1 is [`stage1_candidates`] (survival plus two other dependent
/// interactions, composite-ordered); every later hop is the stage's own
/// evidence verdict. It holds no state and adds no arithmetic.
#[derive(Clone, Copy, Debug, Default)]
pub struct SwarmScorer;

impl PromotionScorer for SwarmScorer {
    fn candidates(
        &self,
        graph: &Graph,
        scores: &ScoreTable,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Vec<NodeId> {
        stage1_candidates(graph, scores, params.min_age, now)
    }

    fn admits_hop(
        &self,
        _graph: &Graph,
        _node: NodeId,
        _to: CanonizationStatus,
        _params: &EvalParams,
        _now: DateTime<Utc>,
    ) -> bool {
        // Swarm has no score of its own: the stage verdict, which the
        // evaluator has already required, is the decision.
        true
    }
}

/// The four band edges of the solo score, compared **after** the
/// eviction-resistance multiplier and **inclusive** (`>=`): a resistant score
/// of exactly 3.0 is a Candidate, exactly 6.0 a Venerable, exactly 10.0 a
/// Canonical. Anything below [`CANDIDATE_BAR`] is ordinary memory.
///
/// All three bars are consumed, not merely published: [`CANDIDATE_BAR`] gates
/// Stage-1 admission ([`SoloScorer::candidates`]), and the upper two gate the
/// Candidate → Venerable and Venerable → Canonical hops
/// ([`SoloScorer::admits_hop`] — the ladder cannot lift a concept past its
/// band).
pub const CANDIDATE_BAR: f64 = 3.0;
/// See [`CANDIDATE_BAR`].
pub const VENERABLE_BAR: f64 = 6.0;
/// See [`CANDIDATE_BAR`].
pub const CANONICAL_BAR: f64 = 10.0;

/// Event-time separation that makes two touches of a concept distinct
/// "sessions" in the recurrence term (spec §3.2's "separated by ≥ 24 hours").
pub const SESSION_SEPARATION: Duration = Duration::from_secs(86_400);

/// Spec §3.2 term weights.
pub const SESSION_WEIGHT: f64 = 1.0;
/// See [`SESSION_WEIGHT`].
pub const HUMAN_CONFIRMED_WEIGHT: f64 = 4.0;
/// Per distinct dependent interaction (#36 PR 3). Spec §3.2's weight for
/// "valid actions", which this term replaces: a dependence declared from
/// another interaction is what a valid action against a ruling looks like.
pub const DEPENDENT_INTERACTION_WEIGHT: f64 = 2.0;
/// Subtracted per revert.
pub const REVERT_WEIGHT: f64 = 3.0;

/// How far down the canonization ladder a status is (None lowest).
fn status_rank(s: CanonizationStatus) -> u8 {
    match s {
        CanonizationStatus::None => 0,
        CanonizationStatus::Candidate => 1,
        CanonizationStatus::Venerable => 2,
        CanonizationStatus::Canonical => 3,
    }
}

/// Resolved about-times of every interaction that supported this concept:
/// direct derivations (`Derives` from an interaction), plus the writing
/// interaction of every concept that declared it depends on this one
/// (`Dependency` into it). Sorted, deduplicated — this is the recurrence
/// term's input, and it resolves through `about_time`, never flush stamps.
///
/// `Causal` (an action produced or modified this concept) is **not** support
/// since #36 PR 3, the same reason it confers no load (#36 PR 1): a file
/// modified on five separate days recurred as work, not as a fact anyone
/// rests on, and that alone made files Solo Candidates. This is the spec
/// §3.2 errata recorded in `dev-diary/notes/feature-36-pr3-admission.md`.
///
/// A fact with no event time contributes its flush time (D's fallback rule),
/// so a live session behaves exactly as it did pre-D: one cluster.
pub fn supporting_interaction_times(graph: &Graph, c: &Concept) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    for e in graph.incident_edges(c.id) {
        if e.target != c.id {
            continue;
        }
        let writer = match e.edge_type {
            EdgeType::Derives => Some(e.source),
            EdgeType::Dependency => match graph.node(e.source) {
                // record_action sources `depends_on` from the action node (a
                // Resource concept); credit that action's turn.
                Some(Node::Concept(s)) => Some(s.origin_interaction),
                _ => None,
            },
            _ => None,
        };
        if let Some(id) = writer
            && let Some(Node::Interaction(i)) = graph.node(id)
        {
            times.push(i.about_time());
        }
    }
    times.sort();
    times.dedup();
    times
}

/// The recurrence term: how many ≥ [`SESSION_SEPARATION`]-apart sessions the
/// concept's supporting interactions fall into, counted by D2's greedy
/// [`separated_session_count`] on event time.
pub fn recurrence_sessions(graph: &Graph, c: &Concept) -> usize {
    separated_session_count(&supporting_interaction_times(graph, c), SESSION_SEPARATION)
}

/// How many times this concept lost standing — every audited transition whose
/// `to_status` ranks strictly below its `from_status`. The write gate
/// (`legal_canonization_transition`, behind
/// [`crate::graph::Graph::apply_canonization_transition`]) admits exactly one
/// such rank-*decreasing* transition, Canonical → None: stage skips and downgrades
/// (`Venerable → None`, `Canonical → Venerable`, …) are rejected there, so
/// the filter counts Canonical → None demotions, **whatever their reason**:
/// budget demotions and, since #20 (with `canonization_supersede` on),
/// supersessions too. A superseded concept therefore carries Solo's revert
/// penalty, as a budget-demoted one always has (#20 review L5). It is
/// written as a rank comparison on purpose — any future addition to the
/// legal set that decreases rank lands in this count by the same filter,
/// which is the behavior an audit of that change must re-examine. Reads the
/// durable canonization-event log; no new state.
pub fn revert_count(graph: &Graph, id: NodeId) -> usize {
    graph
        .canonization_events()
        .iter()
        .filter(|ev| ev.node_id == id && status_rank(ev.to_status) < status_rank(ev.from_status))
        .count()
}

/// The solo score's terms for one concept, before and after the type
/// multiplier. Inspect explains a Solo verdict with these.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SoloBreakdown {
    /// ≥ 24 h-separated sessions among the supporting interactions.
    pub sessions: usize,
    /// Human confirmations.
    pub human_confirmed: i32,
    /// Distinct dependent interactions other than the concept's own, aged.
    pub dependent_interactions: usize,
    /// Rank-decreasing transitions in the audit.
    pub reverts: usize,
    /// The concept type's eviction-resistance multiplier.
    pub resistance: f64,
    /// The resistant score the bands compare.
    pub score: f64,
    /// The band that score falls in.
    pub band: CanonizationStatus,
}

/// [`SoloBreakdown`] for `c`, given its dependent-interaction count (from
/// [`dependent_interaction_counts`] or [`dependent_interactions`]).
pub fn solo_breakdown(graph: &Graph, c: &Concept, dependent_interactions: usize) -> SoloBreakdown {
    let sessions = recurrence_sessions(graph, c);
    let reverts = revert_count(graph, c.id);
    let raw = sessions as f64 * SESSION_WEIGHT
        + c.human_confirmed as f64 * HUMAN_CONFIRMED_WEIGHT
        + dependent_interactions as f64 * DEPENDENT_INTERACTION_WEIGHT
        - reverts as f64 * REVERT_WEIGHT;
    let resistance = c.concept_type.eviction_resistance();
    let score = raw * resistance;
    SoloBreakdown {
        sessions,
        human_confirmed: c.human_confirmed,
        dependent_interactions,
        reverts,
        resistance,
        score,
        band: classify(score),
    }
}

/// The raw §3.2 sum, before the type multiplier.
pub fn raw_solo_score(graph: &Graph, c: &Concept, dependent_interactions: usize) -> f64 {
    recurrence_sessions(graph, c) as f64 * SESSION_WEIGHT
        + c.human_confirmed as f64 * HUMAN_CONFIRMED_WEIGHT
        + dependent_interactions as f64 * DEPENDENT_INTERACTION_WEIGHT
        - revert_count(graph, c.id) as f64 * REVERT_WEIGHT
}

/// The solo score with eviction resistance applied.
///
/// ## Reconciliation with the daemon's additive `concept_type_modifier`
///
/// Two conversions of the same v0.6.0 per-type table serve two different
/// formulas, and neither duplicates the other:
///
/// * **Daemon composite** ([`crate::daemon::score`]): spec §9 defines an
///   *additive* bonus term on a `[0, 1 + MAX_BONUS]` composite, so the table's
///   score-multiplier column enters as an offset. It feeds GC thresholds and
///   ranking.
/// * **Solo score** (here): spec §3.2 defines resistance as a *multiplier* on
///   an unbounded evidence sum, so the table's eviction-resistance column
///   enters multiplicatively via
///   [`crate::types::ConceptType::eviction_resistance`] (Constraint 1.5 …
///   Observation 0.7), applied before the band comparison.
pub fn solo_score(graph: &Graph, c: &Concept, dependent_interactions: usize) -> f64 {
    raw_solo_score(graph, c, dependent_interactions) * c.concept_type.eviction_resistance()
}

/// Band classification of a **resistant** score. Inclusive at every bar
/// (`>=`): the boundary values themselves belong to the higher band, so a
/// score of exactly [`CANONICAL_BAR`] is Canonical while one epsilon below is
/// Venerable. Tested at all three boundaries below.
///
/// This is the policy's ladder admission predicate: [`SoloScorer::candidates`]
/// reduces Stage 1 to `classify(score) != None`, and
/// [`SoloScorer::admits_hop`] admits a hop to `to` exactly when the node's
/// band ranks at or above `to`.
pub fn classify(resistant: f64) -> CanonizationStatus {
    if resistant >= CANONICAL_BAR {
        CanonizationStatus::Canonical
    } else if resistant >= VENERABLE_BAR {
        CanonizationStatus::Venerable
    } else if resistant >= CANDIDATE_BAR {
        CanonizationStatus::Candidate
    } else {
        CanonizationStatus::None
    }
}

/// Single-writer promotion (C2; evidence-gated since #36 PR 3).
///
/// Stage 1 admits every concept whose resistant score clears the Candidate
/// bar, ordered by that score (highest first), then canonical key, then
/// `NodeId`. The later hops need the node's band **and** the stage verdict:
/// the bands cap how high a concept can climb, the verdicts decide whether it
/// may climb at all. The Canonical budget and the stage-3 re-promotion
/// cooldown apply as for every policy; the revert penalty is what cools a
/// demoted concept's score down.
#[derive(Clone, Copy, Debug, Default)]
pub struct SoloScorer;

impl PromotionScorer for SoloScorer {
    fn candidates(
        &self,
        graph: &Graph,
        _scores: &ScoreTable,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Vec<NodeId> {
        let counts = dependent_interaction_counts(graph, params.min_age, now);
        let mut admitted: Vec<(f64, &Concept)> = graph
            .concepts()
            .map(|c| {
                let deps = counts.get(&c.id).copied().unwrap_or(0);
                (solo_score(graph, c, deps), c)
            })
            .filter(|(score, _)| classify(*score) != CanonizationStatus::None)
            .collect();
        admitted.sort_by(|(sa, a), (sb, b)| {
            sb.total_cmp(sa).then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
        });
        admitted.into_iter().map(|(_, c)| c.id).collect()
    }

    fn admits_hop(
        &self,
        graph: &Graph,
        node: NodeId,
        to: CanonizationStatus,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> bool {
        match graph.node(node) {
            Some(Node::Concept(c)) => {
                let deps = dependent_interactions(graph, node, params.min_age, now);
                status_rank(classify(solo_score(graph, c, deps))) >= status_rank(to)
            }
            // Not a concept (or gone): nothing to admit.
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Interaction, Scored, SessionId,
    };
    use chrono::TimeZone;
    use uuid::Uuid;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    /// `ALL` is what every refusal message enumerates, so a variant missing
    /// from it is a refusal that hides the value the operator needed.
    ///
    /// The coverage half is not asserted here, and deliberately so: `ALL` and
    /// the enum are generated from one variant list by `promotion_policy!`, so
    /// a variant `ALL` does not name is unrepresentable rather than untested.
    /// T3-P3-2: the assertion that *used* to stand in for coverage —
    /// `ALL.len() == 2` — was vacuous in exactly the direction that mattered,
    /// because the length moves only when a variant is added to `ALL` and the
    /// loop below iterates `ALL`, so a variant absent from `ALL` was reached by
    /// neither. Restoring any `ALL.len() == N` literal here would restore that,
    /// and would also give the generated length something to disagree with.
    ///
    /// What is left to test is the part construction does *not* give: that every
    /// value in the set survives the round trip an operator's config actually
    /// takes — `as_str` out, `from_str` back — and that `expected()` names it.
    #[test]
    fn all_covers_every_variant_and_round_trips_through_from_str() {
        for policy in PromotionPolicy::ALL {
            assert_eq!(
                policy.as_str().parse::<PromotionPolicy>(),
                Ok(policy),
                "{policy:?} must parse back from its own config spelling"
            );
            assert_eq!(policy.to_string(), policy.as_str());
            assert!(PromotionPolicy::expected().contains(policy.as_str()));
        }
    }

    /// Trimmed and case-insensitive, like `StoreKind::from_str` and
    /// `EmbedderKind::from_str` — `lambo.toml`'s every other value is
    /// snake_case, so `"solo"` is what an operator types. Still fail-closed:
    /// nothing unrecognized silently becomes the default.
    #[test]
    fn from_str_is_lenient_about_shape_and_strict_about_membership() {
        for (raw, want) in [
            ("Swarm", PromotionPolicy::Swarm),
            ("swarm", PromotionPolicy::Swarm),
            ("SWARM", PromotionPolicy::Swarm),
            ("  sWaRm\t", PromotionPolicy::Swarm),
            ("Solo", PromotionPolicy::Solo),
            ("solo", PromotionPolicy::Solo),
            ("\n SOLO ", PromotionPolicy::Solo),
        ] {
            assert_eq!(raw.parse::<PromotionPolicy>(), Ok(want), "{raw:?}");
        }
        for raw in ["", "   ", "\t\n", "Solitary", "swarms", "sol o", "Both"] {
            let err = raw
                .parse::<PromotionPolicy>()
                .expect_err(&format!("{raw:?} must be refused, never defaulted"));
            for needle in ["Swarm", "Solo"] {
                assert!(
                    err.contains(needle),
                    "the refusal for {raw:?} must name {needle}: {err}"
                );
            }
        }
        // The rejected value is quoted back, so an operator can see the typo
        // (a bare valid-set list leaves them guessing which key was wrong).
        assert!(
            "Solitary"
                .parse::<PromotionPolicy>()
                .unwrap_err()
                .contains("Solitary"),
            "the refusal must quote what it rejected"
        );
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn concept(id: u64, gc: i32) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: NodeId(Uuid::from_u64_pair(1, 1)),
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: gc,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        }
    }

    /// Pillar `1` (origin `i1`, `gc_survived = gc`) cited through `Dependency`
    /// by one action from each of interactions `i2` and `i3`, all an hour
    /// before [`ts`]; plus a second concept `2` with no dependents.
    fn evidence_graph(gc: i32) -> (Graph, ScoreTable) {
        let mut g = Graph::new(sid());
        let mut prev = None;
        for n in 1..=3u64 {
            let mut turn = interaction_at(n, -3600 + n as i64, None);
            turn.previous_id = prev;
            prev = Some(turn.id);
            g.insert_interaction(turn).unwrap();
        }
        g.insert_concept(concept(1, gc), nid_inter(1)).unwrap();
        g.insert_concept(concept(2, gc), nid_inter(1)).unwrap();
        for n in 2..=3u64 {
            let mut action = concept(10 + n, 0);
            action.origin_interaction = nid_inter(n);
            g.insert_concept(action, nid_inter(n)).unwrap();
            g.upsert_edge(test_edge(
                n,
                nid(10 + n),
                nid(1),
                EdgeType::Dependency,
                ts_at(-3000),
            ))
            .unwrap();
        }
        let scores = ScoreTable {
            epoch: 0,
            // Concept 2 outscores the pillar: the score orders, never selects.
            ranked: vec![Scored::new(nid(2), 9.0), Scored::new(nid(1), 0.1)],
        };
        (g, scores)
    }

    fn params() -> EvalParams {
        EvalParams::default()
    }

    // -----------------------------------------------------------------------
    // C2 helpers — solo-score corpora
    // -----------------------------------------------------------------------

    fn ts_at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000 + secs, 0).unwrap()
    }

    /// An interaction flushed at `flushed_secs`, optionally ABOUT
    /// `event_time`.
    fn interaction_at(
        id: u64,
        flushed_secs: i64,
        event_time: Option<DateTime<Utc>>,
    ) -> Interaction {
        Interaction {
            event_time,
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("turn {id}")),
            previous_id: None,
            created_at: ts_at(flushed_secs),
        }
    }

    fn test_edge(
        id: u64,
        src: NodeId,
        tgt: NodeId,
        ty: EdgeType,
        at: DateTime<Utc>,
    ) -> crate::types::Edge {
        crate::types::Edge {
            id: NodeId(Uuid::from_u64_pair(3, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: 0.5,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
            event_time: None,
        }
    }

    fn day(n: u64) -> DateTime<Utc> {
        ts_at((n * 86_400) as i64)
    }

    /// A hub concept derived from interaction 1 and additionally supported by
    /// interactions `supporters` through hand-upserted `Derives` edges (the
    /// shape a replayed ingest produces: every later turn re-derived the fact).
    /// Every interaction is flushed back-to-back inside one minute — the bulk
    /// bootstrap — while their ABOUT-times are `day(0)`, `day(2)`, `day(4)`,
    /// `day(6)`: real event-time spread, none in flush time.
    fn spread_support_graph(hub_type: ConceptType) -> (Graph, NodeId) {
        spread_support_graph_with(hub_type, true)
    }

    /// As above; `with_event_time = false` strips the about-times so the SAME
    /// rows read on flush time alone (the pre-D / ingest-time reading).
    fn spread_support_graph_with(hub_type: ConceptType, with_event_time: bool) -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let mut prev = None;
        for n in 1..=4u64 {
            let et = with_event_time.then(|| {
                if n == 1 {
                    day(0)
                } else {
                    day(n - 1) + chrono::Duration::hours(48)
                }
            });
            // The temporal chain is a graph invariant: each turn links back.
            let mut turn = interaction_at(n, (n as i64 - 1) * 15, et);
            turn.previous_id = prev;
            let iid = turn.id;
            g.insert_interaction(turn).unwrap();
            prev = Some(iid);
        }
        let mut hub = concept(10, 0);
        hub.concept_type = hub_type;
        let iid1 = nid_inter(1);
        g.insert_concept(hub.clone(), iid1).unwrap();
        // Later turns re-derived the same fact: Derives i_n -> hub.
        for n in 2..=4u64 {
            g.upsert_edge(test_edge(
                n,
                nid_inter(n),
                hub.id,
                EdgeType::Derives,
                ts_at((n as i64 - 1) * 15),
            ))
            .unwrap();
        }
        (g, hub.id)
    }

    fn nid_inter(n: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, n))
    }

    /// The default policy is swarm — the whole contract of C1.
    ///
    /// Mutation: flip `#[default]` to `Solo`, or make `Config::default()`
    /// name `Solo`. Goes red here and in `config`'s `defaults_match_spec`.
    #[test]
    fn the_default_policy_is_swarm() {
        assert_eq!(PromotionPolicy::default(), PromotionPolicy::Swarm);
        assert_eq!(
            crate::Config::default().promotion_policy,
            PromotionPolicy::Swarm
        );
        assert_eq!(params().promotion_policy, PromotionPolicy::Swarm);
    }

    /// The swarm arm reproduces Stage 1's evidence floor, asserted against a
    /// hand-computed expected set rather than against `stage1_candidates`
    /// itself: the pillar with two other dependent interactions is admitted
    /// in a three-interaction graph with no peer population, and the
    /// higher-scoring concept with no dependents is not.
    ///
    /// Mutation: make `SwarmScorer::candidates` return `Vec::new()`, or select
    /// by score again. Both red.
    #[test]
    fn the_swarm_arm_admits_on_evidence_not_score() {
        let (graph, scores) = evidence_graph(3);
        let got = PromotionPolicy::Swarm
            .scorer()
            .candidates(&graph, &scores, &params(), ts());
        assert_eq!(got, vec![nid(1)]);
    }

    /// `gc_survived < 3` fails Stage 1 whatever the evidence, carried
    /// through the seam, not just through the free function.
    #[test]
    fn the_swarm_arm_still_enforces_gc_survived_through_the_seam() {
        let (graph, scores) = evidence_graph(2);
        let got = PromotionPolicy::Swarm
            .scorer()
            .candidates(&graph, &scores, &params(), ts());
        assert!(got.is_empty(), "{got:?}");
    }

    /// Dispatch is real AND solo is real at once: the same event-time-spread
    /// corpus admits the hub under `Solo` and admits nothing under `Swarm`
    /// (one writer, `gc_survived == 0` — Stage 1's swarm gate refuses it). A
    /// mutant that points the `Solo` arm of `scorer()` at `&SwarmScorer`
    /// collapses this to the empty set — exactly the silent-fallback defect
    /// C1's should_panic test guarded, still guarded, without a panic to lean
    /// on now that solo resolves.
    ///
    /// Mutation: map `Solo → &SwarmScorer`, or make [`SoloScorer::candidates`]
    /// return `Vec::new()` (the C1-era forbidden stub). Both red here.
    #[test]
    fn solo_admits_what_swarm_refuses_on_the_same_corpus() {
        let (graph, hub) = spread_support_graph(ConceptType::Entity);
        let scores = ScoreTable::default();
        let got = PromotionPolicy::Solo
            .scorer()
            .candidates(&graph, &scores, &params(), ts_at(60));
        assert_eq!(got, vec![hub]);

        let empty =
            PromotionPolicy::Swarm
                .scorer()
                .candidates(&graph, &scores, &params(), ts_at(60));
        assert!(empty.is_empty(), "swarm must refuse the same corpus");
    }

    /// The four bands, AT their boundaries. Comparisons are inclusive (`>=`)
    /// on the resistant score: exactly 3.0/6.0/10.0 belong to the higher band;
    /// one epsilon below does not.
    ///
    /// Mutation: any `>=` relaxed to `>` or a bar nudged → the exact-equality
    /// arm of the matching assertion goes red.
    #[test]
    fn band_boundaries_are_inclusive() {
        use crate::types::CanonizationStatus::*;
        // Below Candidate.
        assert_eq!(classify(2.99), None);
        assert_eq!(classify(2.999_999), None);
        assert_eq!(classify(0.0), None);
        assert_eq!(classify(-4.0), None, "a reverted score classifies as None");
        // The Candidate bar, both sides.
        assert_eq!(classify(3.0 - 1e-9), None);
        assert_eq!(classify(3.0), Candidate);
        assert_eq!(classify(3.5), Candidate);
        // The Venerable bar, both sides.
        assert_eq!(classify(6.0 - 1e-9), Candidate);
        assert_eq!(classify(6.0), Venerable);
        assert_eq!(classify(9.99), Venerable);
        // The Canonical bar, both sides.
        assert_eq!(classify(10.0 - 1e-9), Venerable);
        assert_eq!(classify(10.0), Canonical);
        assert_eq!(classify(13.5), Canonical);
    }

    /// The eviction-resistance multiplier applies per concept type BEFORE the
    /// band comparison, on identical evidence: three ≥24h-apart sessions
    /// (raw 3.0) admit an Entity (3 × 1.2 = 3.6) but not an Observation
    /// (3 × 0.7 = 2.1) — the weakest kind is the last promoted.
    ///
    /// Mutation: drop the multiplier in `solo_score` → both classify alike and
    /// the Observation half goes red; apply the daemon's additive modifier
    /// instead (+0.15 max: 3.15) → the Observation still clears, red again.
    #[test]
    fn eviction_resistance_multiplies_before_the_band_comparison() {
        let (entity_graph, entity) = spread_support_graph(ConceptType::Entity);
        assert_eq!(
            raw_solo_score(&entity_graph, &concept_of(&entity_graph, entity), 0),
            4.0,
            "four spread sessions, nothing else"
        );
        assert_eq!(
            solo_score(&entity_graph, &concept_of(&entity_graph, entity), 0),
            4.8,
            "Entity resistance 1.2"
        );

        let (obs_graph, obs) = spread_support_graph(ConceptType::Observation);
        let scored = solo_score(&obs_graph, &concept_of(&obs_graph, obs), 0);
        assert!(
            (scored - 2.8).abs() < 1e-9,
            "Observation resistance 0.7, got {scored}"
        );
        let admitted = PromotionPolicy::Solo.scorer().candidates(
            &obs_graph,
            &ScoreTable::default(),
            &params(),
            ts_at(60),
        );
        assert!(
            admitted.is_empty(),
            "2.8 < 3.0: an Observation stays ordinary memory"
        );
    }

    /// Exact-boundary landings through the full formula (not just `classify`):
    /// integer evidence × the type multiplier lands ON a bar, and the
    /// inclusive comparison admits it.
    #[test]
    fn integer_evidence_lands_exactly_on_bars() {
        // Resource ×1.0 keeps every raw value exact.
        let mut g = Graph::new(sid());
        let mut prev = None;
        for n in 1..=4u64 {
            let mut turn = interaction_at(n, (n * 15) as i64, Some(day(n - 1)));
            turn.previous_id = prev;
            let iid = turn.id;
            g.insert_interaction(turn).unwrap();
            prev = Some(iid);
        }
        let mut hub = concept(10, 0);
        hub.concept_type = ConceptType::Resource;
        hub.human_confirmed = 1;
        g.insert_concept(hub.clone(), nid_inter(1)).unwrap();
        for n in 2..=4u64 {
            g.upsert_edge(test_edge(
                n,
                nid_inter(n),
                hub.id,
                EdgeType::Derives,
                ts_at((n * 15) as i64),
            ))
            .unwrap();
        }
        // Sessions 4×1 + confirmed 1×4 + dependents 0 − reverts 0 = 8.0.
        let c = concept_of(&g, hub.id);
        assert!((solo_score(&g, &c, 0) - 8.0).abs() < 1e-9);
        assert_eq!(
            classify(solo_score(&g, &c, 0)),
            CanonizationStatus::Venerable
        );

        // One dependent interaction (an action from interaction 2 declaring
        // `depends_on` the hub): 8 + 2 = 10.0 EXACTLY, Canonical inclusively.
        let mut action = concept(20, 0);
        action.concept_type = ConceptType::Resource;
        action.content = "deploy api".into();
        action.canonical_key = action.content.clone();
        action.origin_interaction = nid_inter(2);
        let aid = action.id;
        g.insert_concept(action, nid_inter(2)).unwrap();
        g.upsert_edge(test_edge(30, aid, hub.id, EdgeType::Dependency, ts_at(90)))
            .unwrap();
        let later = day(10);
        let deps = dependent_interactions(&g, hub.id, Duration::from_secs(60), later);
        assert_eq!(deps, 1);
        let c = concept_of(&g, hub.id);
        assert!((solo_score(&g, &c, deps) - 10.0).abs() < 1e-9);
        assert_eq!(
            classify(solo_score(&g, &c, deps)),
            CanonizationStatus::Canonical
        );
    }

    /// Human confirmations enter only through the confirm verb, and each bump
    /// is worth 4.0: zero confirmations leave recurrence alone to carry the
    /// score; two take the same corpus from Candidate-band to Venerable-band.
    #[tokio::test]
    async fn human_confirmations_are_real_and_weighted() {
        let (mut graph, hub) = spread_support_graph(ConceptType::Resource); // ×1.0
        let c = concept_of(&graph, hub);
        assert!((raw_solo_score(&graph, &c, 0) - 4.0).abs() < 1e-9);

        assert_eq!(graph.confirm_human(hub).unwrap(), 1);
        assert_eq!(graph.confirm_human(hub).unwrap(), 2);
        let c = concept_of(&graph, hub);
        assert_eq!(c.human_confirmed, 2);
        // 4×1 + 2×4 = 12.0 — Canonical band.
        assert!((solo_score(&graph, &c, 0) - 12.0).abs() < 1e-9);
        assert_eq!(
            classify(solo_score(&graph, &c, 0)),
            CanonizationStatus::Canonical
        );

        // The bumps were mutations: draining the log yields them for the
        // flush pipeline to persist.
        let batch = graph.drain_log();
        assert!(
            batch.mutations.iter().any(|m| matches!(
                m,
                crate::types::Mutation::UpsertNode { node } if node.id() == hub
            )),
            "confirm_human must append an UpsertNode mutation"
        );

        // A missing node fails loudly instead of vanishing.
        assert!(graph
            .confirm_human(NodeId(Uuid::from_u64_pair(9, 9)))
            .is_err());
    }

    /// The dependent term counts DISTINCT other interactions: two actions
    /// from one interaction count once, an action writing both `Causal` and
    /// `Dependency` counts once, and `Causal` alone (an artifact an action
    /// touched) counts nothing. Under spec §3.2's valid-action term the
    /// `Causal` toucher scored 2.0; that is how Solo canonized file paths.
    #[test]
    fn dependent_interactions_dedup_and_ignore_causal_touches() {
        let (mut g, hub) = spread_support_graph(ConceptType::Entity);
        let later = day(10);
        let age = Duration::from_secs(60);
        assert_eq!(dependent_interactions(&g, hub, age, later), 0, "Derives");
        let add_action = |g: &mut Graph, id: u64, origin: u64, ty: EdgeType| {
            let mut action = concept(id, 0);
            action.concept_type = ConceptType::Resource;
            action.content = format!("action {id}");
            action.canonical_key = action.content.clone();
            action.origin_interaction = nid_inter(origin);
            g.insert_concept(action, nid_inter(origin)).unwrap();
            g.upsert_edge(test_edge(id, nid(id), hub, ty, ts_at(90)))
                .unwrap();
        };
        add_action(&mut g, 20, 2, EdgeType::Causal);
        assert_eq!(
            dependent_interactions(&g, hub, age, later),
            0,
            "Causal confers no load"
        );
        add_action(&mut g, 21, 3, EdgeType::Dependency);
        add_action(&mut g, 22, 3, EdgeType::Dependency);
        assert_eq!(
            dependent_interactions(&g, hub, age, later),
            1,
            "two actions from one interaction count once"
        );
        add_action(&mut g, 23, 4, EdgeType::Dependency);
        assert_eq!(dependent_interactions(&g, hub, age, later), 2);
        // The hub's own interaction never counts.
        add_action(&mut g, 24, 1, EdgeType::Dependency);
        assert_eq!(dependent_interactions(&g, hub, age, later), 2);
    }

    /// #36 PR 3 (spec §3.2 errata): a `Causal` touch is not recurrence.
    /// A second concept derived once in turn 1 and modified by actions from
    /// turns 2, 3 and 4 (days 2/4/6) still has one supporting session; the
    /// same actions declaring `depends_on` it give four.
    #[test]
    fn causal_touches_are_not_recurrence() {
        let (mut g, _) = spread_support_graph(ConceptType::Resource);
        let mut file = concept(40, 0);
        file.concept_type = ConceptType::Resource;
        file.content = "src/lib.rs".into();
        file.canonical_key = file.content.clone();
        g.insert_concept(file.clone(), nid_inter(1)).unwrap();
        for (k, ty) in [
            (2u64, EdgeType::Causal),
            (3, EdgeType::Causal),
            (4, EdgeType::Causal),
        ] {
            let mut action = concept(40 + k, 0);
            action.concept_type = ConceptType::Resource;
            action.content = format!("modify lib {k}");
            action.canonical_key = action.content.clone();
            action.origin_interaction = nid_inter(k);
            g.insert_concept(action, nid_inter(k)).unwrap();
            g.upsert_edge(test_edge(40 + k, nid(40 + k), file.id, ty, ts_at(90)))
                .unwrap();
        }
        assert_eq!(recurrence_sessions(&g, &concept_of(&g, file.id)), 1);
        for k in 2..=4u64 {
            g.upsert_edge(test_edge(
                50 + k,
                nid(40 + k),
                file.id,
                EdgeType::Dependency,
                ts_at(90),
            ))
            .unwrap();
        }
        assert_eq!(recurrence_sessions(&g, &concept_of(&g, file.id)), 4);
    }

    /// Opposition is not a dependent or another supporting turn.
    #[test]
    fn contradictions_do_not_supply_solo_support_or_dependents() {
        let (mut graph, hub) = spread_support_graph(ConceptType::Entity);
        let before = supporting_interaction_times(&graph, &concept_of(&graph, hub));
        let mut action = concept(20, 0);
        action.concept_type = ConceptType::Resource;
        action.content = "contrary action".into();
        action.canonical_key = action.content.clone();
        action.origin_interaction = nid_inter(2);
        graph.insert_concept(action.clone(), nid_inter(2)).unwrap();
        graph
            .upsert_edge(test_edge(
                30,
                action.id,
                hub,
                EdgeType::Contradicts,
                ts_at(90),
            ))
            .unwrap();
        graph
            .upsert_edge(test_edge(
                31,
                hub,
                action.id,
                EdgeType::Contradicts,
                ts_at(90),
            ))
            .unwrap();
        assert_eq!(
            dependent_interactions(&graph, hub, Duration::ZERO, day(10)),
            0
        );
        assert_eq!(
            supporting_interaction_times(&graph, &concept_of(&graph, hub)),
            before
        );
    }

    /// #20 review L5: a supersession is a Canonical → None demotion, so it
    /// counts toward Solo's revert penalty exactly like a budget demotion.
    #[test]
    fn a_supersession_counts_as_a_revert() {
        let (mut graph, hub) = spread_support_graph(ConceptType::Resource);
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            graph
                .apply_canonization_transition(transition(hub, from, to))
                .unwrap();
        }
        graph
            .apply_canonization_transition(crate::types::CanonizationEvent {
                reason: Some(crate::types::DemotionReason::Superseded { by: NodeId::new() }),
                ..transition(hub, CanonizationStatus::Canonical, CanonizationStatus::None)
            })
            .unwrap();
        assert_eq!(revert_count(&graph, hub), 1);
    }

    /// Reverts subtract: one demotion (−3.0) pulls a three-session fact back
    /// under the bar. Only rank-DECREASING transitions count; promotions do
    /// not.
    #[test]
    fn reverts_subtract_and_promotions_do_not_count() {
        let (mut graph, hub) = spread_support_graph(ConceptType::Resource);
        assert_eq!(revert_count(&graph, hub), 0);

        // The full climb — every promotion hop, none a revert.
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            graph
                .apply_canonization_transition(transition(hub, from, to))
                .unwrap();
            assert_eq!(revert_count(&graph, hub), 0, "{from:?} -> {to:?} climbs");
        }

        // Budget/conflict-style demotion: the state machine's one way down.
        graph
            .apply_canonization_transition(transition(
                hub,
                CanonizationStatus::Canonical,
                CanonizationStatus::None,
            ))
            .unwrap();
        assert_eq!(revert_count(&graph, hub), 1);
        // 4×1 − 1×3 = 1.0 → ordinary memory again.
        let c = concept_of(&graph, hub);
        assert!((raw_solo_score(&graph, &c, 0) - 1.0).abs() < 1e-9);
        assert!(PromotionPolicy::Solo
            .scorer()
            .candidates(&graph, &ScoreTable::default(), &params(), ts_at(60))
            .is_empty());
    }

    /// DONE-WHEN BOX 3, measured: the SAME physical corpus recurs under event
    /// time and does NOT recur under ingest time. Four supporting turns about
    /// days 0/2/4/6, all flushed inside one minute, evaluated pinned right
    /// after the burst.
    ///
    /// * Event time: about-times sit ≥48h apart → 4 distinct sessions →
    ///   Entity 4.8 → admitted.
    /// * Ingest time (same rows, event times stripped — the pre-D reading):
    ///   flush stamps span 45 seconds → ONE session → 1.2 → refused.
    ///
    /// Mutations: resolve about-time as bare `created_at` (drop D's fallback
    /// rule) or ignore event time entirely — either collapses the first half
    /// onto the second's numbers and the paired assertion goes red.
    #[test]
    fn a_bulk_ingest_recurs_only_under_event_time() {
        // Event-timed corpus: admitted.
        let (graph, hub) = spread_support_graph(ConceptType::Entity);
        let times = supporting_interaction_times(&graph, &concept_of(&graph, hub));
        assert_eq!(
            separated_session_count(&times, SESSION_SEPARATION),
            4,
            "days 0/2/4/6 are four ≥24h-apart sessions"
        );
        assert!(PromotionPolicy::Solo
            .scorer()
            .candidates(&graph, &ScoreTable::default(), &params(), ts_at(60))
            .contains(&hub));

        // Same rows, event times stripped: refused.
        let (stripped, hub) = spread_support_graph_with(ConceptType::Entity, false);
        let times = supporting_interaction_times(&stripped, &concept_of(&stripped, hub));
        assert_eq!(
            separated_session_count(&times, SESSION_SEPARATION),
            1,
            "flush stamps within one minute are ONE session"
        );
        assert!(!PromotionPolicy::Solo
            .scorer()
            .candidates(&stripped, &ScoreTable::default(), &params(), ts_at(60))
            .contains(&hub));
    }

    /// C2 discrimination: on a MIXED corpus, does SoloPolicy admit the fact
    /// that genuinely recurs and refuse the two shapes that only look busy?
    ///
    /// The existing `a_bulk_ingest_recurs_only_under_event_time` proves the
    /// mechanism — one hub, admitted with event time, refused without. It
    /// cannot answer the question an operator actually has, because a corpus
    /// containing only a passing concept cannot show a failing one being
    /// turned away. This builds all three shapes into ONE graph and asserts
    /// the candidate set exactly, so a policy that admitted everything would
    /// be as red as one that admitted nothing.
    ///
    /// The three shapes, and why these three:
    ///
    /// * `recurring` — re-derived on days 0/2/4/6. Four sessions ≥24h apart:
    ///   the signal SoloPolicy exists to read, and the only one of the three
    ///   a human would call load-bearing.
    /// * `burst` — re-derived the SAME number of times (four), all inside one
    ///   afternoon. Identical support count, no separation. This is the
    ///   "wrote a bunch about what we just did" shape, and separating it from
    ///   `recurring` is the whole reason the recurrence term is defined over
    ///   time rather than over count.
    /// * `once` — derived a single time and never revisited.
    ///
    /// Mutations: define recurrence over support COUNT instead of separated
    /// sessions (admits `burst`, red); drop the separation floor to 0 (admits
    /// both, red); require separation of the origin interaction only rather
    /// than of the supporting set (admits `once`, red).
    #[test]
    fn solo_admits_the_recurring_fact_and_refuses_the_burst_and_the_one_off() {
        let mut g = Graph::new(sid());

        // Interactions 1..=4: spread across days 0/2/4/6 (real event time),
        // all flushed within minutes of each other (the bootstrap shape).
        let mut prev = None;
        for n in 1..=4u64 {
            let et = if n == 1 {
                day(0)
            } else {
                day(n - 1) + chrono::Duration::hours(48)
            };
            let mut turn = interaction_at(n, (n as i64 - 1) * 15, Some(et));
            turn.previous_id = prev;
            prev = Some(turn.id);
            g.insert_interaction(turn).unwrap();
        }
        // Interactions 5..=7: all on day 0, one afternoon — the burst.
        for n in 5..=7u64 {
            let et = day(0) + chrono::Duration::minutes(20 * (n as i64 - 4));
            let mut turn = interaction_at(n, (n as i64 - 1) * 15, Some(et));
            turn.previous_id = prev;
            prev = Some(turn.id);
            g.insert_interaction(turn).unwrap();
        }

        // The three concepts, each anchored on interaction 1.
        let mut recurring = concept(10, 0);
        recurring.concept_type = ConceptType::Constraint;
        g.insert_concept(recurring.clone(), nid_inter(1)).unwrap();

        let mut burst = concept(11, 0);
        burst.concept_type = ConceptType::Constraint;
        g.insert_concept(burst.clone(), nid_inter(1)).unwrap();

        let mut once = concept(12, 0);
        once.concept_type = ConceptType::Constraint;
        g.insert_concept(once.clone(), nid_inter(1)).unwrap();

        // `recurring` is re-derived by the day-spread turns 2,3,4.
        for n in 2..=4u64 {
            g.upsert_edge(test_edge(
                n,
                nid_inter(n),
                recurring.id,
                EdgeType::Derives,
                ts_at((n as i64 - 1) * 15),
            ))
            .unwrap();
        }
        // `burst` is re-derived exactly as often — by the same-day turns 5,6,7.
        for n in 5..=7u64 {
            g.upsert_edge(test_edge(
                n + 10,
                nid_inter(n),
                burst.id,
                EdgeType::Derives,
                ts_at((n as i64 - 1) * 15),
            ))
            .unwrap();
        }
        // `once` gets nothing further.

        // Support counts are deliberately equal, so a count-based rule cannot
        // tell these two apart and only a time-based one can.
        let rec_times = supporting_interaction_times(&g, &concept_of(&g, recurring.id));
        let burst_times = supporting_interaction_times(&g, &concept_of(&g, burst.id));
        assert_eq!(
            rec_times.len(),
            burst_times.len(),
            "the two must be indistinguishable by support COUNT, or this test proves nothing"
        );
        assert_eq!(separated_session_count(&rec_times, SESSION_SEPARATION), 4);
        assert_eq!(separated_session_count(&burst_times, SESSION_SEPARATION), 1);

        let got = PromotionPolicy::Solo.scorer().candidates(
            &g,
            &ScoreTable::default(),
            &params(),
            ts_at(60),
        );

        assert!(
            got.contains(&recurring.id),
            "the genuinely recurring fact must be admitted; got {got:?}"
        );
        assert!(
            !got.contains(&burst.id),
            "a same-day burst has no recurrence however many times it was re-derived"
        );
        assert!(
            !got.contains(&once.id),
            "a fact derived once is not recurrence"
        );
    }

    // -----------------------------------------------------------------------
    // C2 helpers continued — small accessors over test graphs
    // -----------------------------------------------------------------------

    fn concept_of(graph: &Graph, id: NodeId) -> Concept {
        match graph.node(id) {
            Some(crate::types::Node::Concept(c)) => c.clone(),
            other => panic!("not a concept: {other:?}"),
        }
    }

    fn transition(
        node: NodeId,
        from: CanonizationStatus,
        to: CanonizationStatus,
    ) -> crate::types::CanonizationEvent {
        crate::types::CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: node,
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(),
            reason: None,
        }
    }

    /// The config representation is `PascalCase`, matching `MatchStrategy`.
    ///
    /// Mutation: drop `#[serde(rename_all = "PascalCase")]` → the variants
    /// serialize as `"Swarm"`/`"Solo"` anyway (they are already PascalCase in
    /// Rust), so this test is deliberately written against the *wire* text a
    /// config file carries, and additionally pins that lowercase is refused —
    /// which is what actually changes if the attribute is swapped for
    /// `snake_case`.
    #[test]
    fn the_config_representation_is_pascal_case() {
        assert_eq!(
            serde_json::to_string(&PromotionPolicy::Swarm).unwrap(),
            "\"Swarm\""
        );
        assert_eq!(
            serde_json::to_string(&PromotionPolicy::Solo).unwrap(),
            "\"Solo\""
        );
        assert_eq!(
            serde_json::from_str::<PromotionPolicy>("\"Solo\"").unwrap(),
            PromotionPolicy::Solo
        );
        // The DERIVE stays exact — this is `Config`'s JSON, a library wire
        // format nobody hand-types. The *config-file* surface is deliberately
        // lenient and does not go through the derive: `LamboFile`'s
        // `promotion_policy` uses `deserialize_config_value`, which routes to
        // `from_str` so `lambo.toml` accepts what `LAMBO_PROMOTION_POLICY`
        // accepts. The two are not in tension; they are two audiences.
        assert!(serde_json::from_str::<PromotionPolicy>("\"solo\"").is_err());
    }
    // -----------------------------------------------------------------------
    // C-R1-1 closure — the published bands drive the ladder admission
    // -----------------------------------------------------------------------

    /// Under solo, hop admission is the node's band (#36 PR 3: on top of the
    /// stage verdict the evaluator requires first; see
    /// `solo_cannot_bypass_stage_2_on_its_band` for that half). A Resource
    /// hub with four event-timed sessions sits at resistant 4.0: the band
    /// refuses Venerable. One human confirmation lifts it to 8.0 (Venerable
    /// band, not Canonical). One dependent interaction lands it on exactly
    /// [`CANONICAL_BAR`] (10.0), which the inclusive comparison admits.
    ///
    /// Mutation: the rank comparison made exclusive (`>`) → red at 10.0.
    #[test]
    fn solo_admission_climbs_with_the_band() {
        let (mut graph, hub) = spread_support_graph_with(ConceptType::Resource, true);
        let solo = PromotionPolicy::Solo.scorer();
        let p = params();
        let at = day(10);
        assert!(solo.admits_hop(&graph, hub, CanonizationStatus::Candidate, &p, at));
        assert!(
            !solo.admits_hop(&graph, hub, CanonizationStatus::Venerable, &p, at),
            "4.0 < 6.0: below the bar the band refuses whatever the evidence"
        );

        graph.confirm_human(hub).unwrap();
        assert!(solo.admits_hop(&graph, hub, CanonizationStatus::Venerable, &p, at));
        assert!(!solo.admits_hop(&graph, hub, CanonizationStatus::Canonical, &p, at));

        let mut actor = concept(11, 0);
        actor.origin_interaction = nid_inter(2);
        let actor_id = actor.id;
        graph.insert_concept(actor, nid_inter(2)).unwrap();
        graph
            .upsert_edge(test_edge(20, actor_id, hub, EdgeType::Dependency, ts()))
            .unwrap();
        let hub_concept = concept_of(&graph, hub);
        let deps = dependent_interactions(&graph, hub, p.min_age, at);
        assert_eq!(solo_score(&graph, &hub_concept, deps), 10.0);
        assert!(solo.admits_hop(&graph, hub, CanonizationStatus::Canonical, &p, at));
    }

    /// The swarm arm adds nothing to the stage verdict the evaluator has
    /// already required: it admits every hop it is asked about.
    #[test]
    fn swarm_admission_is_the_evidence_verdict() {
        let (graph, hub) = spread_support_graph(ConceptType::Entity);
        let swarm = PromotionPolicy::Swarm.scorer();
        let (p, at) = (params(), ts());
        assert!(swarm.admits_hop(&graph, hub, CanonizationStatus::Venerable, &p, at));
        assert!(swarm.admits_hop(&graph, hub, CanonizationStatus::Canonical, &p, at));
    }
}
