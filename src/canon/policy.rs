//! Promotion policy selection — the `PromotionScorer` seam (C1).
//!
//! Canonization's Stage 1 is the one place the **swarm assumption** is welded
//! in. Spec §3.2's design has independent agents converging on the same fact;
//! Stage 1 encodes that as a session gate on peer count plus a cut at the P90
//! of the non-Canonical peer score distribution
//! ([`crate::canon::stage1_candidates`]). With a single writer there are no
//! independent peers, nothing converges, and nothing is ever promoted — so the
//! graph never produces the load-bearing warnings that are its whole point.
//!
//! Stages 2 and 3 are **not** part of this seam. They measure evidence
//! (`interaction_span`, `blast_radius`) rather than agreement, and evidence
//! means the same thing whether one writer or twenty produced it. What breaks
//! them at bootstrap is the *clock*, not the peer model, which is workstream D
//! and not this one.
//!
//! ## What C1 was, and what C2 added
//!
//! C1 was the **seam only**: the policy becomes selectable, and swarm stays
//! the default so no existing behaviour moves. Its scorer refused to resolve
//! — an empty-set stub would have been indistinguishable at runtime from a
//! finished, broken SoloPolicy.
//!
//! C2 lands that scorer. The solo score (spec §3.2) is
//!
//! ```text
//! (Sessions × 1.0) + (Human Confirmed × 4.0) + (Valid Actions × 2.0)
//!     − (Reverts × 3.0)
//! ```
//!
//! multiplied by the concept-type eviction-resistance table
//! ([`crate::types::ConceptType::eviction_resistance`]) before the band
//! comparison: ≥ [`CANONICAL_BAR`] Canonical, ≥ [`VENERABLE_BAR`] Venerable,
//! ≥ [`CANDIDATE_BAR`] Candidate, else None. Three of the four inputs derive
//! from structures the graph already carries; the fourth (`human_confirmed`)
//! is a persisted counter bumped only through
//! [`crate::Memory::confirm_human`] — see the field docs on
//! [`crate::types::Concept::human_confirmed`] for the data-model decision.
//!
//! The recurrence term is the reason C2 depends on D2: "sessions" are counted
//! with [`separated_session_count`] over the **event-time** spread of the
//! concept's supporting interactions ([`crate::types::Interaction::about_time`],
//! never flush stamps). A bulk ingest of a decade of history in ninety
//! minutes has that separation in commit dates and none whatsoever in flush
//! time; built against ingest time this policy would promote nothing — and
//! ship a passing test proving it works. The test that pins the difference
//! is `a_bulk_ingest_recurs_only_under_event_time` below.
//!
//! ## Shape: enum on the params, trait for the behaviour
//!
//! The selector rides on [`crate::canon::EvalParams`], which is already "the
//! knobs for one cycle" and already carries `min_peer_count`, `min_age`,
//! `min_edge_age`, `cooldown`, `batch_size` and `max_canonical_nodes`. Nothing
//! here duplicates one of those: the policy chooses *which predicate reads
//! them*, it does not restate a threshold.
//!
//! [`Evaluator::gather`](crate::canon::Evaluator) therefore keeps its
//! signature — the seam cost the default path one enum field and one
//! indirection, not a refactor.
//!
//! ## Injected clock
//!
//! [`PromotionScorer::candidates`] takes `now`, and the solo scorer reads
//! event time from the graph's rows ([`crate::types::Interaction::about_time`])
//! rather than from any clock. Nothing here reaches for `Utc::now()` — the
//! same injected-clock discipline `gate.rs` keeps, unbroken across the seam.
//!
//! `now` is currently unused by the solo scorer's arithmetic: every term it
//! reads is a stored instant or a stored count. The parameter stays because
//! the trait is shared with the swarm arm and because the first solo-specific
//! *age* term (if one is ever tuned) must enter through it, not through the
//! wall clock.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canon::event_time::separated_session_count;
use crate::canon::stage1_candidates;
use crate::canon::EvalParams;
use crate::daemon::ScoreTable;
use crate::graph::Graph;
use crate::types::{CanonizationStatus, Concept, EdgeType, Node, NodeId};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum PromotionPolicy {
    /// Spec §3.2 multi-agent convergence: peer-count session gate, P90 cut on
    /// the non-Canonical peer score distribution. The default, and the only
    /// policy a session can currently run under.
    #[default]
    Swarm,
    /// Single-writer promotion (spec §3.2's solo score).
    ///
    /// Implemented as of C2: [`SoloScorer`] computes the §3.2 formula over the
    /// graph's own evidence. The default remains [`PromotionPolicy::Swarm`] —
    /// nothing canonizes under solo unless a session opts in.
    Solo,
}

impl PromotionPolicy {
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


    /// The value as it is written in config, for error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionPolicy::Swarm => "Swarm",
            PromotionPolicy::Solo => "Solo",
        }
    }
}

/// The Stage-1 promotion decision, as a policy.
///
/// One method, because Stage 1 is the only stage whose predicate encodes *how
/// agreement is established*. Stages 2 and 3 ask the store for evidence and
/// are policy-independent (see the module docs).
///
/// The upper two bands of that score are published here
/// ([`classify`], [`VENERABLE_BAR`], [`CANONICAL_BAR`]) and boundary-tested,
/// but their *pipeline* consumption point stays open: Stages 2 and 3 measure
/// evidence (`interaction_span`, blast radius), which is policy-independent
/// by C1's design decision, so a solo session climbs Candidate → Venerable →
/// Canonical through the same evidence gates after this scorer admits it at
/// the Candidate bar. If solo ever grows its own Stage-2/3 predicate, the
/// bars it must compare against already live in exactly one place — here.
pub trait PromotionScorer: Send + Sync + std::fmt::Debug {
    /// Concepts that currently clear the Candidate bar, `NodeId` ascending.
    ///
    /// `now` is injected rather than read from the clock — see the module
    /// docs on D2 compatibility. [`SwarmScorer`] ignores it.
    fn candidates(
        &self,
        graph: &Graph,
        scores: &ScoreTable,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Vec<NodeId>;
}

/// Spec §3.2 multi-agent convergence — the shipped policy.
///
/// A pure delegation to [`stage1_candidates`]. It holds no state and adds no
/// arithmetic: the seam must be able to prove it changed nothing, and the
/// cheapest proof is that the default arm still calls the same function with
/// the same argument.
#[derive(Clone, Copy, Debug, Default)]
pub struct SwarmScorer;

impl PromotionScorer for SwarmScorer {
    fn candidates(
        &self,
        graph: &Graph,
        scores: &ScoreTable,
        params: &EvalParams,
        _now: DateTime<Utc>,
    ) -> Vec<NodeId> {
        stage1_candidates(graph, scores, params.min_peer_count)
    }
}
// ---------------------------------------------------------------------------
// The solo score (spec §3.2) — C2
// ---------------------------------------------------------------------------

/// Event-time separation that makes two touches of a concept distinct
/// "sessions" in the recurrence term (spec §3.2's "separated by ≥ 24 hours").
pub const SESSION_SEPARATION: Duration = Duration::from_secs(86_400);

/// The four band edges of the solo score, compared **after** the
/// eviction-resistance multiplier and **inclusive** (`>=`): a resistant score
/// of exactly 3.0 is a Candidate, exactly 6.0 a Venerable, exactly 10.0 a
/// Canonical. Anything below [`CANDIDATE_BAR`] is ordinary memory.
pub const CANDIDATE_BAR: f64 = 3.0;
/// See [`CANDIDATE_BAR`].
pub const VENERABLE_BAR: f64 = 6.0;
/// See [`CANDIDATE_BAR`].
pub const CANONICAL_BAR: f64 = 10.0;

/// Spec §3.2 term weights, verbatim.
pub const SESSION_WEIGHT: f64 = 1.0;
/// See [`SESSION_WEIGHT`].
pub const HUMAN_CONFIRMED_WEIGHT: f64 = 4.0;
/// See [`SESSION_WEIGHT`].
pub const VALID_ACTION_WEIGHT: f64 = 2.0;
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

/// Resolved about-times of every interaction whose work touched this concept:
/// direct derivations (`Derives` from an interaction), plus the writing
/// interaction of every action concept that recorded a `Causal`/`Dependency`
/// edge against it. Sorted, deduplicated — this is the recurrence term's
/// input, and it resolves through `about_time`, never flush stamps.
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
            EdgeType::Causal | EdgeType::Dependency => match graph.node(e.source) {
                // record_action sources its structural edges from the action
                // node (a Resource concept); credit that action's turn.
                Some(Node::Concept(s)) => Some(s.origin_interaction),
                _ => None,
            },
            _ => None,
        };
        if let Some(id) = writer {
            if let Some(Node::Interaction(i)) = graph.node(id) {
                times.push(i.about_time());
            }
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

/// Distinct recorded actions against this concept — spec §3.2's "valid
/// actions". An action *is* valid here because recording it already ran the
/// full write-time validation pipeline ([`crate::graph::action::record_action`]:
/// canonicalization, cycle check); a rejected action leaves no edge and no
/// trace, so there is nothing to subtract. Counted per **source** concept, so
/// one action producing and depending on the same thing counts once.
pub fn valid_action_count(graph: &Graph, c: &Concept) -> usize {
    let mut sources: HashSet<NodeId> = HashSet::new();
    for e in graph.incident_edges(c.id) {
        if e.target == c.id
            && matches!(e.edge_type, EdgeType::Causal | EdgeType::Dependency)
            && matches!(graph.node(e.source), Some(Node::Concept(_)))
        {
            sources.insert(e.source);
        }
    }
    sources.len()
}

/// How many times this concept lost standing — every audited transition whose
/// `to_status` ranks strictly below its `from_status` (a demotion to any lower
/// rung: budget demotion Canonical → None, conflict demotion Venerable → None,
/// a partial Canonical → Venerable step-down). Reads the durable
/// canonization-event log; no new state.
pub fn revert_count(graph: &Graph, id: NodeId) -> usize {
    graph
        .canonization_events()
        .iter()
        .filter(|ev| {
            ev.node_id == id && status_rank(ev.to_status) < status_rank(ev.from_status)
        })
        .count()
}

/// The raw §3.2 sum, before the type multiplier.
pub fn raw_solo_score(graph: &Graph, c: &Concept) -> f64 {
    recurrence_sessions(graph, c) as f64 * SESSION_WEIGHT
        + c.human_confirmed as f64 * HUMAN_CONFIRMED_WEIGHT
        + valid_action_count(graph, c) as f64 * VALID_ACTION_WEIGHT
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
///   score-multiplier column enters as an offset,
///   `concept_type_modifier = score_multiplier() − 1.0`, clamped to
///   `±[−0.10, +0.15]`. It feeds GC thresholds and ranking.
/// * **Solo score** (here): spec §3.2 defines resistance as a *multiplier* on
///   an unbounded evidence sum, so the table's eviction-resistance column
///   enters multiplicatively via the existing
///   [`crate::types::ConceptType::eviction_resistance`] (Constraint 1.5 …
///   Observation 0.7). Applied before the band comparison, which is what makes
///   Constraints promote first and Observations last under equal evidence —
///   the point of calling them eviction-*resistant*.
///
/// Both read their numbers from the two `const fn`s on
/// [`crate::types::ConceptType`]: exactly one table per column exists in
/// `src/`, no parallel knob was added, and neither scorer writes anything the
/// other reads.
pub fn solo_score(graph: &Graph, c: &Concept) -> f64 {
    raw_solo_score(graph, c) * c.concept_type.eviction_resistance()
}

/// Band classification of a **resistant** score. Inclusive at every bar
/// (`>=`): the boundary values themselves belong to the higher band, so a
/// score of exactly [`CANONICAL_BAR`] is Canonical while one epsilon below is
/// Venerable. Tested at all three boundaries below.
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

/// Single-writer promotion — implemented (C2).
///
/// Computes the §3.2 score for every concept in the session and admits those
/// whose resistant score clears the Candidate bar, `NodeId` ascending (the
/// order `Evaluator::gather` expects). Still-`None` concepts only reach the
/// promotion path anyway — `gather` re-filters on current status before the
/// batch cut — so this predicate needs no state beyond the graph.
///
/// The C1-era shape was a deliberate `unimplemented!()` backstop behind
/// `Config::validate`'s Solo refusal: an empty-set stub would have been
/// indistinguishable at runtime from a finished policy that promoted nothing.
/// That reasoning cost nothing here — the refusal was loud until the formula
/// was real, and removing both gate and backstop together is this commit's
/// clean cutover.
#[derive(Clone, Copy, Debug, Default)]
pub struct SoloScorer;

impl PromotionScorer for SoloScorer {
    fn candidates(
        &self,
        graph: &Graph,
        _scores: &ScoreTable,
        _params: &EvalParams,
        _now: DateTime<Utc>,
    ) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = graph
            .concepts()
            .filter(|c| classify(solo_score(graph, c)) != CanonizationStatus::None)
            .map(|c| c.id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
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

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn interaction() -> Interaction {
        Interaction {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(1, 1)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("prompt".into()),
            previous_id: None,
            created_at: ts(),
        }
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
            chunk_group_id: None,
        }
    }

    /// Twenty peers scoring `1.0 ..= 20.0`, all with `gc_survived == 3`.
    ///
    /// Nearest-rank P90 over `n = 20` is `ceil(0.9 × 20) = 18` → the 18th
    /// smallest score, `18.0`. Strictly-above leaves exactly the two
    /// top-scoring concepts, ids 19 and 20.
    fn graph_with(concepts: impl IntoIterator<Item = Concept>) -> Graph {
        let mut g = Graph::new(sid());
        let i = interaction();
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        for c in concepts {
            g.insert_concept(c, iid).unwrap();
        }
        g
    }

    fn twenty_peer_graph() -> (Graph, ScoreTable) {
        let graph = graph_with((1..=20u64).map(|i| concept(i, 3)));
        let scores = ScoreTable {
            epoch: 0,
            ranked: (1..=20u64).map(|i| Scored::new(nid(i), i as f64)).collect(),
        };
        (graph, scores)
    }

    /// The same twenty peers, with one concept's `gc_survived` overridden.
    fn twenty_peer_graph_with_gc(target: u64, gc: i32) -> (Graph, ScoreTable) {
        let graph = graph_with((1..=20u64).map(|i| concept(i, if i == target { gc } else { 3 })));
        let scores = ScoreTable {
            epoch: 0,
            ranked: (1..=20u64).map(|i| Scored::new(nid(i), i as f64)).collect(),
        };
        (graph, scores)
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

    /// The swarm arm reproduces Stage 1 exactly, asserted against a
    /// hand-computed expected set rather than against `stage1_candidates`
    /// itself — comparing the dispatch to the function it calls is a
    /// tautology that survives every mutation of either.
    ///
    /// Mutation: make `SwarmScorer::candidates` return `Vec::new()`, or drop
    /// the `gc_survived` term, or relax `>` to `>=` in `stage1`. All red.
    #[test]
    fn the_swarm_arm_cuts_at_p90_of_twenty_peers() {
        let (graph, scores) = twenty_peer_graph();
        let got = PromotionPolicy::Swarm
            .scorer()
            .candidates(&graph, &scores, &params(), ts());
        assert_eq!(got, vec![nid(19), nid(20)]);
    }

    /// `gc_survived < 3` fails Stage 1 even at the top of the distribution —
    /// carried through the seam, not just through the free function.
    ///
    /// Mutation: drop the `gc_survived` conjunct in `stage1_candidates` →
    /// id 20 reappears and this goes red.
    #[test]
    fn the_swarm_arm_still_enforces_gc_survived_through_the_seam() {
        let (graph, scores) = twenty_peer_graph_with_gc(20, 2);
        let got = PromotionPolicy::Swarm
            .scorer()
            .candidates(&graph, &scores, &params(), ts());
        assert_eq!(got, vec![nid(19)]);
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

        let empty = PromotionPolicy::Swarm
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
    /// (3 × 0.7 = 2.1) — the weakest kind is the first evicted, never the
    /// first promoted.
    ///
    /// Mutation: drop the multiplier in `solo_score` → both classify alike and
    /// the Observation half goes red; apply the daemon's additive modifier
    /// instead (+0.15 max: 3.15) → the Observation still clears, red again.
    #[test]
    fn eviction_resistance_multiplies_before_the_band_comparison() {
        let (entity_graph, entity) = spread_support_graph(ConceptType::Entity);
        assert_eq!(
            raw_solo_score(&entity_graph, &concept_of(&entity_graph, entity)),
            4.0,
            "four spread sessions, nothing else"
        );
        assert_eq!(
            solo_score(&entity_graph, &concept_of(&entity_graph, entity)),
            4.8,
            "Entity resistance 1.2"
        );

        let (obs_graph, obs) = spread_support_graph(ConceptType::Observation);
        let scored = solo_score(&obs_graph, &concept_of(&obs_graph, obs));
        assert!((scored - 2.8).abs() < 1e-9, "Observation resistance 0.7, got {scored}");
        let admitted = PromotionPolicy::Solo
            .scorer()
            .candidates(&obs_graph, &ScoreTable::default(), &params(), ts_at(60));
        assert!(admitted.is_empty(), "2.8 < 3.0: an Observation stays ordinary memory");
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
            let mut turn =
                interaction_at(n, (n * 15) as i64, Some(day((n - 1) as u64)));
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
        // Sessions 4×1 + confirmed 1×4 + actions 0 − reverts 0 = 8.0 … not yet.
        let c = concept_of(&g, hub.id);
        assert!((solo_score(&g, &c) - 8.0).abs() < 1e-9);
        assert_eq!(classify(solo_score(&g, &c)), CanonizationStatus::Venerable);

        // One valid action (an action node with a Causal edge): 8 + 2 = 10.0
        // EXACTLY — Canonical, inclusively.
        let mut action = concept(20, 0);
        action.concept_type = ConceptType::Resource;
        action.content = "deploy api".into();
        action.canonical_key = action.content.clone();
        let aid = action.id;
        g.insert_concept(action, nid_inter(1)).unwrap();
        g.upsert_edge(test_edge(
            30,
            aid,
            hub.id,
            EdgeType::Causal,
            ts_at(90),
        ))
        .unwrap();
        let c = concept_of(&g, hub.id);
        assert_eq!(valid_action_count(&g, &c), 1);
        assert!((solo_score(&g, &c) - 10.0).abs() < 1e-9);
        assert_eq!(classify(solo_score(&g, &c)), CanonizationStatus::Canonical);
    }

    /// Human confirmations enter only through the confirm verb, and each bump
    /// is worth 4.0: zero confirmations leave recurrence alone to carry the
    /// score; two take the same corpus from Candidate-band to Venerable-band.
    #[tokio::test]
    async fn human_confirmations_are_real_and_weighted() {
        let (mut graph, hub) = spread_support_graph(ConceptType::Resource); // ×1.0
        let c = concept_of(&graph, hub);
        assert!((raw_solo_score(&graph, &c) - 4.0).abs() < 1e-9);

        assert_eq!(graph.confirm_human(hub).unwrap(), 1);
        assert_eq!(graph.confirm_human(hub).unwrap(), 2);
        let c = concept_of(&graph, hub);
        assert_eq!(c.human_confirmed, 2);
        // 4×1 + 2×4 = 12.0 — Canonical band.
        assert!((solo_score(&graph, &c) - 12.0).abs() < 1e-9);
        assert_eq!(classify(solo_score(&graph, &c)), CanonizationStatus::Canonical);

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
        assert!(graph.confirm_human(NodeId(Uuid::from_u64_pair(9, 9))).is_err());
    }

    /// Valid actions count DISTINCT action concepts once each, even when one
    /// action wrote two edges against the target (produces AND depends_on).
    #[test]
    fn valid_actions_dedup_by_action_node() {
        let (graph, hub) = spread_support_graph(ConceptType::Entity);
        // Two more interactions so dedup assertions don't disturb sessions.
        let c = concept_of(&graph, hub);
        assert_eq!(valid_action_count(&graph, &c), 0, "Derives are not actions");

        let mut g = graph;
        let mut action = concept(20, 0);
        action.concept_type = ConceptType::Resource;
        action.content = "record act".into();
        action.canonical_key = action.content.clone();
        g.insert_concept(action.clone(), nid_inter(1)).unwrap();
        g.upsert_edge(test_edge(30, action.id, hub, EdgeType::Causal, ts_at(90)))
            .unwrap();
        g.upsert_edge(test_edge(31, action.id, hub, EdgeType::Dependency, ts_at(90)))
            .unwrap();
        assert_eq!(
            valid_action_count(&g, &concept_of(&g, hub)),
            1,
            "one action, two edges — counted once"
        );
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
        assert!((raw_solo_score(&graph, &c) - 1.0).abs() < 1e-9);
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

    // -----------------------------------------------------------------------
    // C2 helpers continued — small accessors over test graphs
    // -----------------------------------------------------------------------

    fn concept_of(graph: &Graph, id: NodeId) -> Concept {
        match graph.node(id) {
            Some(crate::types::Node::Concept(c)) => c.clone(),
            other => panic!("not a concept: {other:?}"),
        }
    }

    fn transition(node: NodeId, from: CanonizationStatus, to: CanonizationStatus) -> crate::types::CanonizationEvent {
        crate::types::CanonizationEvent {
            id: NodeId::new(),
            session_id: sid(),
            node_id: node,
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(),
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
        assert!(serde_json::from_str::<PromotionPolicy>("\"solo\"").is_err());
    }
}
