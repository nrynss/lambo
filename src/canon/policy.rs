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
//! ## What C1 is, and what it deliberately is not
//!
//! C1 is the **seam only**: the policy becomes selectable, and swarm stays the
//! default so no existing behaviour moves. [`PromotionPolicy::Solo`] is
//! declared and dispatched to, but its scorer **refuses to resolve** — see
//! [`SoloScorer`] for why that refusal is the deliberate shape rather than a
//! placeholder formula.
//!
//! C2 — the actual solo score from spec §3.2, its four thresholds, and the
//! concept-type eviction-resistance multipliers — **depends on D2** (event
//! time) and is not implemented here. The dependency is easy to miss and
//! expensive to discover: the solo formula's recurrence term wants three or
//! more sessions separated by at least 24 hours, and a bulk ingest of a decade
//! of history in ninety minutes has no such separation. Built against ingest
//! time, C2 promotes nothing — and ships a passing test that proves it works.
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
//! ## D2 compatibility
//!
//! [`PromotionScorer::candidates`] takes `now` even though [`SwarmScorer`]
//! ignores it. Stage 1 has no time term today, so this parameter buys nothing
//! *now*; it exists so that D2 can hand the solo policy event time without
//! widening the trait, and so the eventual solo scorer reads an injected clock
//! rather than reaching for `Utc::now()`. `gate.rs` already takes `now` as an
//! argument rather than calling the clock itself — this keeps that discipline
//! unbroken across the new seam.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canon::stage1_candidates;
use crate::canon::EvalParams;
use crate::daemon::ScoreTable;
use crate::graph::Graph;
use crate::types::NodeId;

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
    /// **Declared, not implemented.** [`crate::Config::validate`] refuses this
    /// value, so no session can start under it; if that gate is somehow
    /// bypassed, [`SoloScorer`] aborts rather than promoting on a guess. See
    /// [`SoloScorer`] for the reasoning, and C2 in
    /// `dev-diary/lambo-for-mooshik/C-solopolicy.md` for the work it is
    /// waiting on.
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

    /// Whether this policy can actually run a cycle.
    ///
    /// The one place the "which policies are implemented" question is
    /// answered, so [`crate::Config::validate`] and the docs cannot drift
    /// apart from the dispatch table above.
    pub fn is_implemented(self) -> bool {
        match self {
            PromotionPolicy::Swarm => true,
            PromotionPolicy::Solo => false,
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
/// C2 is expected to **add** methods here — the solo score assigns Venerable
/// and Canonical bars of its own, which today's Stage 2 and Stage 3 own. That
/// widening is left to C2 on purpose: the solo formula's inputs (session
/// recurrence, human confirmations, valid actions, reverts) do not exist in
/// the data model yet, and a trait method shaped around data that does not
/// exist is fiction, not a seam.
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

/// Single-writer promotion — **declared, and deliberately unimplemented.**
///
/// ## Why this refuses instead of scoring
///
/// The obvious placeholder — return an empty set until C2 lands — is the one
/// shape that must not be built. "Promoted nothing" is precisely the symptom
/// of the bug C2's D2 dependency exists to prevent, so an empty-set stub is
/// **indistinguishable at runtime from a finished, broken SoloPolicy**. It
/// would sit behind a green test suite, promote nothing for the same reason
/// the real thing would, and give no signal at all that the formula was
/// missing rather than merely unsatisfied.
///
/// The other tempting shape — a thin plausible formula, some fraction of the
/// spec §3.2 score — is worse. It would read as C2, be measured as C2, and
/// quietly launder a guess into the write-up as a tuned result.
///
/// So this refuses. A caller that reaches here has bypassed
/// [`crate::Config::validate`], which means the process is running a policy
/// nobody implemented; aborting the cycle is the honest outcome and the
/// canonization task already contains panics
/// (`CanonizationTask`'s `CatchUnwindPoll`) so the loop survives to log it.
///
/// The real gate is `validate`, at startup, with a message that names the
/// missing work. This is the backstop behind it.
#[derive(Clone, Copy, Debug, Default)]
pub struct SoloScorer;

impl PromotionScorer for SoloScorer {
    fn candidates(
        &self,
        _graph: &Graph,
        _scores: &ScoreTable,
        _params: &EvalParams,
        _now: DateTime<Utc>,
    ) -> Vec<NodeId> {
        unimplemented!(
            "promotion_policy = \"Solo\" is declared but not implemented (C2 depends on D2, \
             event time); Config::validate refuses this value, so reaching here means that \
             check was bypassed. Refusing to promote rather than returning an empty set, \
             which is indistinguishable from a finished policy that promoted nothing."
        )
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

    /// Dispatch is real: `Solo` must not silently resolve to the swarm
    /// scorer. This is the specific defect the seam could hide — a fallback
    /// that makes `promotion_policy` decorative.
    ///
    /// Mutation: point the `Solo` arm of `scorer()` at `&SwarmScorer`. The
    /// call then succeeds, no panic is raised, and `should_panic` fails the
    /// test. Verified red.
    #[test]
    #[should_panic(expected = "not implemented")]
    fn solo_refuses_to_resolve_rather_than_falling_back_to_swarm() {
        let (graph, scores) = twenty_peer_graph();
        let params = EvalParams {
            promotion_policy: PromotionPolicy::Solo,
            ..EvalParams::default()
        };
        let _ = PromotionPolicy::Solo
            .scorer()
            .candidates(&graph, &scores, &params, ts());
    }

    /// The refusal names the config key and the blocking workstream, so the
    /// abort is actionable rather than a bare `unimplemented!()`.
    #[test]
    fn the_solo_refusal_names_the_key_and_the_dependency() {
        let (graph, scores) = twenty_peer_graph();
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            SoloScorer.candidates(&graph, &scores, &params(), ts())
        }))
        .expect_err("SoloScorer must refuse");
        // `unimplemented!` with no format arguments yields a `&'static str`
        // payload; with arguments it yields a `String`. Read both so the test
        // does not silently pass on an empty message if that ever changes.
        let msg = err
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| err.downcast_ref::<String>().map(String::as_str))
            .expect("panic payload must carry the refusal message");
        assert!(msg.contains("promotion_policy"), "message: {msg}");
        assert!(msg.contains("Solo"), "message: {msg}");
        assert!(msg.contains("D2"), "message: {msg}");
    }

    /// `is_implemented` is the single source the validator reads, so it must
    /// agree with the dispatch table rather than restate it.
    ///
    /// Mutation: make `is_implemented` return `true` for `Solo` → this goes
    /// red, and so does `config`'s refusal test.
    #[test]
    fn only_swarm_reports_itself_implemented() {
        assert!(PromotionPolicy::Swarm.is_implemented());
        assert!(!PromotionPolicy::Solo.is_implemented());
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
