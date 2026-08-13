//! Stage 1 — Candidate (T6.1, spec §10).
//!
//! Pure predicate: reports which concepts currently clear Stage 1. It does
//! **not** apply a `None → Candidate` transition (T6.4 owns writes).
//!
//! A concept passes when every rule below holds:
//!
//! 1. **Session gate.** Evaluated only when the session has at least
//!    `min_peer_count` **non-Canonical** concepts (`status != Canonical`).
//!    Below that threshold the result is empty — nobody passes.
//! 2. **`gc_survived >= 3`.**
//! 3. **Strictly above P90.** The concept's daemon composite (from the
//!    caller's [`ScoreTable`]) is **strictly greater** than the 90th
//!    percentile of non-Canonical peer scores. Exactly-at-P90 does not pass.
//! 4. **Missing [`ScoreTable`] entry → `0.0`**, same as assemble (T5.3).
//!    A non-finite or negative entry is likewise read as `0.0` — the ALGO-10
//!    `sane_weight` convention `recall::assemble` applies to the identical
//!    lookup (see [`sane_score`]).
//!
//! Non-Canonical peers are every concept with `status != Canonical`,
//! **including the candidate** — the session-wide distribution is not
//! leave-one-out. Canonical concepts are neither peers nor candidates.
//!
//! ## P90 definition (nearest-rank)
//!
//! Locked as the **nearest-rank** method (Wikipedia / NIST one-sided rank),
//! not linear interpolation:
//!
//! ```text
//! rank = ceil(0.90 × n)     // 1-based, n = non-Canonical peer count
//! P90  = sorted_ascending[rank - 1]
//! ```
//!
//! For `n = 20`, `rank = 18` so P90 is the 18th smallest score. Linear
//! interpolators (Excel `PERCENTILE.INC` / NumPy default, Hyndman-Fan R7)
//! would yield a non-sample value (`18.1` on `1..=20`) and are rejected by
//! `p90_nearest_rank_is_the_ceil_np_value`.
//!
//! Output is `NodeId` ascending so the set is a total order regardless of
//! `HashMap` walk order.

use std::collections::HashMap;

use crate::daemon::ScoreTable;
use crate::graph::Graph;
#[cfg(test)]
use crate::types::Node;
use crate::types::{CanonizationStatus, NodeId};

/// Stage 1 survival floor (spec §10).
const MIN_GC_SURVIVED: i32 = 3;
/// Percentile of the non-Canonical peer distribution (spec §10).
const PEER_PERCENTILE: f64 = 0.90;

/// Concepts that currently clear Stage 1, in `NodeId` ascending order.
pub fn stage1_candidates(graph: &Graph, scores: &ScoreTable, min_peer_count: usize) -> Vec<NodeId> {
    let peers: Vec<&crate::types::Concept> = graph
        .concepts()
        .filter(|c| c.canonization_status != CanonizationStatus::Canonical)
        .collect();
    if peers.len() < min_peer_count {
        return Vec::new();
    }

    let score_of: HashMap<NodeId, f64> = scores.ranked.iter().map(|s| (s.item, s.score)).collect();
    let daemon_score = |id: NodeId| sane_score(score_of.get(&id).copied().unwrap_or(0.0));

    let mut peer_scores: Vec<f64> = peers.iter().map(|c| daemon_score(c.id)).collect();
    peer_scores.sort_by(|a, b| a.total_cmp(b));
    let p90 = percentile_nearest_rank(&peer_scores, PEER_PERCENTILE);

    let mut passed: Vec<NodeId> = peers
        .iter()
        .filter(|c| c.gc_survived >= MIN_GC_SURVIVED && daemon_score(c.id) > p90)
        .map(|c| c.id)
        .collect();
    passed.sort_by_key(|id| id.0);
    passed
}

/// Finite, non-negative score, else `0.0` — ALGO-10's `sane_weight`
/// convention, applied at the point of use.
///
/// The module claims parity with `recall::assemble`'s treatment of the same
/// `ScoreTable` lookup, and assemble sanitizes. Without this the percentile
/// is not merely wrong but *closing*: `total_cmp` sorts NaN above every real
/// score, so three NaNs among twenty peers push P90 itself to NaN, `score >
/// NaN` is false for everyone, and the stage silently admits nobody. No
/// `rescore` path produces NaN today (every division is guarded), which is
/// why this was P3 — but "unreachable from today's producer" is not the same
/// contract as "sanitized at the point of use", and the doc claimed the
/// latter.
fn sane_score(score: f64) -> f64 {
    if score.is_finite() && score >= 0.0 {
        score
    } else {
        0.0
    }
}

/// Nearest-rank percentile of a **sorted ascending** sample.
///
/// `rank = ceil(p × n)` (1-based), then `sorted[rank - 1]`. An empty sample
/// is not a defined percentile; the session gate is the only caller and
/// never reaches here with `n == 0` unless `min_peer_count == 0`, in which
/// case we treat P90 as `+∞` so nobody can be strictly above it.
fn percentile_nearest_rank(sorted_asc: &[f64], p: f64) -> f64 {
    let n = sorted_asc.len();
    if n == 0 {
        return f64::INFINITY;
    }
    let rank = (p * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted_asc[idx]
}

/// Concept lookup used by tests (Graph exposes [`Graph::node`], not `concept`).
#[cfg(test)]
fn concept_of(graph: &Graph, id: NodeId) -> Option<&crate::types::Concept> {
    match graph.node(id) {
        Some(Node::Concept(c)) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "fixtures")]
    use crate::config::ScoringWeights;
    #[cfg(feature = "fixtures")]
    use crate::daemon::score::rescore;
    use crate::types::{AgentId, Concept, ConceptType, Interaction, Scored, SessionId};
    use chrono::{DateTime, TimeZone, Utc};
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

    fn concept(id: u64, gc: i32, status: CanonizationStatus) -> Concept {
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
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

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

    fn table(pairs: &[(u64, f64)]) -> ScoreTable {
        ScoreTable {
            epoch: 0,
            ranked: pairs
                .iter()
                .map(|&(id, score)| Scored::new(nid(id), score))
                .collect(),
        }
    }

    fn n_none(n: u64, gc: i32) -> Vec<Concept> {
        (1..=n)
            .map(|i| concept(i, gc, CanonizationStatus::None))
            .collect()
    }

    /// Nearest-rank P90 on `1..=n` is `ceil(0.90 n)`, not the R7 interpolate.
    #[test]
    fn p90_nearest_rank_is_the_ceil_np_value() {
        let v20: Vec<f64> = (1..=20).map(|i| i as f64).collect();
        assert_eq!(percentile_nearest_rank(&v20, 0.90), 18.0);
        let v21: Vec<f64> = (1..=21).map(|i| i as f64).collect();
        assert_eq!(percentile_nearest_rank(&v21, 0.90), 19.0);
        // Linear R7 on n=10 is 9.1; nearest-rank is the 9th sample.
        let v10: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        assert_eq!(percentile_nearest_rank(&v10, 0.90), 9.0);
    }

    /// 19 non-Canonical → empty, even when a node has high score + gc >= 3.
    /// Fails if the gate is `> 18` / `>= 19` (off-by-one toward open).
    #[test]
    fn peer_count_gate_19_is_empty_even_with_high_score() {
        let g = graph_with(n_none(19, 5));
        let scores = table(&[(1, 1.0)]);
        assert!(
            stage1_candidates(&g, &scores, 20).is_empty(),
            "19 non-Canonical peers must not open Stage 1"
        );
    }

    /// Complementary lock: 20 non-Canonical with a strictly-above-P90 node
    /// must produce a candidate. Fails if the gate is `>= 21`.
    #[test]
    fn peer_count_gate_20_evaluates() {
        // 19 at 0.1 + one at 1.0; nearest-rank P90 (n=20) is 0.1.
        let g = graph_with(n_none(20, 5));
        let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
        pairs.push((20, 1.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(passed, vec![nid(20)]);
    }

    /// Exactly-at-P90 fails; just-above passes. Fails if the cut is `>=`.
    #[test]
    fn exactly_at_p90_does_not_pass_just_above_does() {
        // n=20, rank=18 → P90 = sorted[17].
        // 17 × 0.1, two × 0.5 (at P90), one × 0.51 (above).
        let g = graph_with(n_none(20, 5));
        let mut pairs: Vec<(u64, f64)> = (1..=17).map(|i| (i, 0.1)).collect();
        pairs.push((18, 0.5));
        pairs.push((19, 0.5));
        pairs.push((20, 0.51));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert!(
            !passed.contains(&nid(18)) && !passed.contains(&nid(19)),
            "exactly-at-P90 must not pass: {passed:?}"
        );
        assert_eq!(passed, vec![nid(20)]);
    }

    /// gc_survived < 3 fails even when the score is strictly above P90.
    #[test]
    fn gc_survived_below_3_fails_even_above_p90() {
        let mut concepts = n_none(19, 5);
        concepts.push(concept(20, 2, CanonizationStatus::None));
        let g = graph_with(concepts);
        let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
        pairs.push((20, 1.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert!(
            !passed.contains(&nid(20)),
            "gc_survived=2 must not pass: {passed:?}"
        );
        assert!(passed.is_empty());
    }

    /// Complementary lock: `gc_survived == 3` passes when the score is
    /// strictly above P90. Fails if the floor is `> 3` / `>= 4`.
    #[test]
    fn gc_survived_3_passes_when_above_p90() {
        let mut concepts = n_none(19, 5);
        concepts.push(concept(20, 3, CanonizationStatus::None));
        let g = graph_with(concepts);
        let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
        pairs.push((20, 1.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(passed, vec![nid(20)]);
    }

    /// Canonicals do not count toward `min_peer_count`. 19 None + N Canonicals
    /// stay closed even when a None has high score + `gc >= 3`. Fails if the
    /// gate counts every concept.
    #[test]
    fn canonicals_do_not_count_toward_peer_gate() {
        let mut concepts = n_none(19, 5);
        concepts.push(concept(20, 5, CanonizationStatus::Canonical));
        concepts.push(concept(21, 5, CanonizationStatus::Canonical));
        concepts.push(concept(22, 5, CanonizationStatus::Canonical));
        let g = graph_with(concepts);
        // Canonical scores stay low so a broken gate is the only way the
        // high-score None (id 19) could pass — not a lifted P90.
        let mut pairs: Vec<(u64, f64)> = (1..=18).map(|i| (i, 0.1)).collect();
        pairs.push((19, 1.0));
        pairs.push((20, 0.0));
        pairs.push((21, 0.0));
        pairs.push((22, 0.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert!(
            passed.is_empty(),
            "19 non-Canonical + Canonicals must not open Stage 1: {passed:?}"
        );
    }

    /// The peer set is "**not** Canonical", not "== None". 19 `None` plus one
    /// `Venerable` is 20 non-Canonical peers, so the session gate opens and a
    /// high-scoring node must still be promoted. Under a `== None` peer set
    /// this session silently stops producing Candidates forever — and the
    /// stage-1 fixture cannot catch it (it holds exactly 20 `None` concepts,
    /// so dropping its one Venerable still leaves the gate open at exactly
    /// 20: pure luck).
    #[test]
    fn nineteen_none_plus_one_venerable_opens_the_session_gate() {
        let mut concepts = n_none(19, 5);
        concepts.push(concept(20, 5, CanonizationStatus::Venerable));
        let g = graph_with(concepts);
        let mut pairs: Vec<(u64, f64)> = (1..=18).map(|i| (i, 0.1)).collect();
        pairs.push((19, 1.0));
        pairs.push((20, 0.1));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(
            passed,
            vec![nid(19)],
            "a Venerable is a non-Canonical peer; the gate must open at 20"
        );
    }

    /// The complement: a Venerable's score enters the P90 **distribution**,
    /// so it moves the cut. 20 `None` + 1 `Venerable` is n=21, rank
    /// `ceil(0.9 × 21) = 19` → P90 is the 19th smallest (0.5), and only the
    /// 0.6 node clears it. Excluding the Venerable gives n=20, rank 18 → P90
    /// is 0.4 and the 0.5 node passes too — a different answer, which is what
    /// makes this discriminating where a `contains` assertion would not be.
    /// The Venerable carries `gc_survived = 0` so it cannot pass itself and
    /// muddy the output.
    #[test]
    fn a_venerable_peers_score_moves_the_p90_cut() {
        let mut concepts = n_none(20, 5);
        concepts.push(concept(21, 0, CanonizationStatus::Venerable));
        let g = graph_with(concepts);
        let mut pairs: Vec<(u64, f64)> = (1..=17).map(|i| (i, 0.1)).collect();
        pairs.push((18, 0.4));
        pairs.push((19, 0.5));
        pairs.push((20, 0.6));
        pairs.push((21, 10.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(
            passed,
            vec![nid(20)],
            "the Venerable's 10.0 must be in the peer distribution, lifting \
             P90 from 0.4 to 0.5 and rejecting the 0.5 node"
        );
    }

    /// Canonicals are not peers (their score must not lift P90) and are not
    /// candidates. Discriminating: 17×0.1 + 0.4 + two×0.5. Correct P90
    /// (n=20, rank 18) is 0.4 so both 0.5s pass; including Canonical 10.0
    /// makes n=21 rank 19 land on 0.5 and rejects them. A unique-max
    /// candidate still clears the lifted P90 (0.45), which is why 0.45
    /// cannot lock this.
    #[test]
    fn canonical_concepts_are_neither_peers_nor_candidates() {
        let mut concepts = n_none(19, 5);
        concepts.push(concept(20, 5, CanonizationStatus::None));
        concepts.push(concept(21, 5, CanonizationStatus::Canonical));
        let g = graph_with(concepts);
        let mut pairs: Vec<(u64, f64)> = (1..=17).map(|i| (i, 0.1)).collect();
        pairs.push((18, 0.4));
        pairs.push((19, 0.5));
        pairs.push((20, 0.5));
        pairs.push((21, 10.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert!(
            !passed.contains(&nid(21)),
            "Canonical must not be a candidate: {passed:?}"
        );
        assert_eq!(
            passed,
            vec![nid(19), nid(20)],
            "Canonical's 10.0 must not enter the peer P90"
        );
        assert_eq!(
            concept_of(&g, nid(21)).unwrap().canonization_status,
            CanonizationStatus::Canonical
        );
    }

    /// A ScoreTable miss is 0.0, not a skip and not an implicit pass.
    #[test]
    fn missing_score_table_entry_is_zero() {
        let g = graph_with(n_none(20, 5));
        // id 20 is absent from the table. 19 zeros + one implicit zero → P90 = 0.
        let pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.0)).collect();
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert!(
            passed.is_empty(),
            "missing score must be 0.0, not above P90=0.0: {passed:?}"
        );
    }

    /// F14: a non-finite score is `0.0` at the point of use (ALGO-10), so it
    /// cannot poison the percentile. Three NaNs among twenty peers sort above
    /// every real score under `total_cmp`, which lands P90 (rank 18 of 20)
    /// on NaN and makes `score > p90` false for everyone — the stage closes
    /// silently. Sanitized, the NaNs sort to the bottom as zeros and the
    /// genuine top scorer still passes.
    #[test]
    fn non_finite_scores_do_not_poison_the_percentile() {
        let g = graph_with(n_none(20, 5));
        let mut pairs: Vec<(u64, f64)> = vec![
            (1, f64::NAN),
            (2, f64::NAN),
            (3, f64::NAN),
            (4, f64::NEG_INFINITY),
        ];
        pairs.extend((5..=19).map(|i| (i, 0.1)));
        pairs.push((20, 1.0));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(
            passed,
            vec![nid(20)],
            "NaN peers must read as 0.0, not close the stage"
        );
    }

    /// Output is NodeId-ascending, not score order.
    #[test]
    fn passed_set_is_id_ascending() {
        let g = graph_with(n_none(20, 5));
        let mut pairs: Vec<(u64, f64)> = (1..=18).map(|i| (i, 0.1)).collect();
        // Higher score on the higher id, so a score-desc sort would flip them.
        pairs.push((19, 0.8));
        pairs.push((20, 0.9));
        let passed = stage1_candidates(&g, &table(&pairs), 20);
        assert_eq!(passed, vec![nid(19), nid(20)]);
    }

    /// Fixture composition smoke: planted `user schema` is Canonical, so it
    /// is not a candidate until rewound. Does not lock P90 arithmetic —
    /// unique-max + `contains` would still pass a loose cut.
    #[cfg(feature = "fixtures")]
    #[test]
    fn rest_api_user_schema_passes_stage1_after_rescore() {
        let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let planted_peers = snap
            .concepts
            .iter()
            .filter(|c| c.canonization_status != CanonizationStatus::Canonical)
            .count();
        assert_eq!(planted_peers, 21, "fixture premise: 21 non-Canonical peers");

        let us_id = snap
            .concepts
            .iter()
            .find(|c| c.content == "user schema")
            .expect("user schema present")
            .id;
        {
            let us = snap
                .concepts
                .iter()
                .find(|c| c.id == us_id)
                .expect("user schema present");
            assert_eq!(us.gc_survived, 4);
            assert_eq!(us.canonization_status, CanonizationStatus::Canonical);
        }

        let min_peers = crate::Config::default().canonization_min_peer_count;
        {
            let g = Graph::from_snapshot(snap.clone()).unwrap();
            let scores = ScoreTable {
                epoch: g.epoch(),
                ranked: rescore(&g, &ScoringWeights::default()),
            };
            let passed = stage1_candidates(&g, &scores, min_peers);
            assert!(
                !passed.contains(&us_id),
                "Canonical user schema must not be a candidate; passed={passed:?}"
            );
        }

        {
            let us = snap
                .concepts
                .iter_mut()
                .find(|c| c.id == us_id)
                .expect("user schema present");
            // End-state stamp: rewind so Stage 1 can name it a candidate.
            us.canonization_status = CanonizationStatus::None;
            us.blast_radius = None;
        }

        let g = Graph::from_snapshot(snap).unwrap();
        let ranked = rescore(&g, &ScoringWeights::default());
        let scores = ScoreTable {
            epoch: g.epoch(),
            ranked,
        };
        let passed = stage1_candidates(&g, &scores, min_peers);
        assert!(
            passed.contains(&us_id),
            "user schema must pass Stage 1 after rescore; passed={passed:?}"
        );
    }
}
