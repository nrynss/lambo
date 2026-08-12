//! Phase-3 scoring, hot-list force-inclusion, and assembly to `max_tokens`
//! (T5.3, spec §8) — the final read-path stage.
//!
//! [`assemble`] turns the phase-2 [`ExpandedSet`] into a [`RecallResult`]:
//! every member (required AND `chunk_group_id` siblings, "scored
//! independently") gets a final score, hot-listed members are force-included
//! after condition re-validation, and the rendered context is truncated to
//! the query's token budget.
//!
//! ## Scoring rule
//!
//! ```text
//! final_score = daemon_score × w_daemon + query_relevance × w_query
//! ```
//!
//! * **query_relevance** — the member's phase-1 candidate score (BM25 for
//!   keyword hits, the max-merged score otherwise). Members that were never
//!   phase-1 candidates — BFS-reached concepts and force-included
//!   `chunk_group_id` siblings — score **0.0**: they were not keyword hits,
//!   so phase 1 has no relevance evidence for them; their only signal is the
//!   daemon score. Siblings are deliberately scored this way (spec §8
//!   "force-included, scored independently"), not silently dropped.
//! * **daemon_score** — the [`ScoreTable`] lookup by node id. A node missing
//!   from the table scores **0.0** (the daemon rescored a different epoch, or
//!   the node was born after the table was computed; a missing entry must not
//!   poison the mix).
//! * **weights** — [`RecallWeights`], sanitized like `ScoringWeights`
//!   (ALGO-10): a non-finite or negative weight becomes `0.0`, so the final
//!   score is finite for every input.
//!
//! Hits are sorted by `final_score` descending, ties broken by node id
//! ascending (the same total order phase 1 uses).
//!
//! ## Hot-list force-include
//!
//! Every expanded member that is on the caller's [`HotList`] is re-validated
//! with [`HotList::revalidate`] **at the caller's `now`** (the same `now`
//! used for everything else in the call: reservations, rendering). The
//! predicate re-derives its recency window from that instant, so an entry
//! whose window elapsed between detection and this read is dropped here
//! (XP-3), and a surviving entry's payload has just been rebuilt against
//! `now` — assemble renders it directly, never a cached copy. Surviving hot
//! members are **force-included**: they stay in the hit list even beyond
//! `top_k`, so a live conflict warning is never truncated away by rank. The
//! per-entry re-validation is a single neighborhood walk (CONC-5), so a
//! handful of hot nodes under the graph lock is cheap.
//!
//! ## Assembly to `max_tokens`
//!
//! The hit list holds the first `top_k` scored members plus every
//! force-included hot member, in final-score order. Each hit renders as a
//! whole block ([`crate::recall::format`]); the context is truncated to
//! `max_tokens` by keeping the **longest score-ordered prefix** of blocks
//! whose cumulative `token_fn` count fits — equivalently, whole
//! lowest-scoring blocks are dropped from the tail. A block is never split,
//! and a dropped block still appears in `RecallResult::hits` and contributes
//! its warnings (a warning is actionable regardless of the token budget; the
//! caller renders `context` and `warnings` separately).
//!
//! The default estimator is [`default_token_count`] (`ceil(bytes / 3.5)`);
//! callers pass their own `Fn(&str) -> usize` to override.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::config::RecallWeights;
use crate::daemon::hotlist::{HotList, HotListPayload};
use crate::daemon::ScoreTable;
use crate::graph::reserve::active_reservation;
use crate::graph::Graph;
use crate::recall::expand::ExpandedSet;
use crate::recall::format;
use crate::types::{
    CanonizationStatus, Node, NodeId, RecallHit, RecallQuery, RecallResult, Scored,
};

/// The built-in token estimator (see [`crate::recall::format`]).
pub use crate::recall::format::default_token_count;

/// Score + assemble the expanded set into the final recall result.
///
/// `phase1` is the phase-1 candidate list (query relevance source), `scores`
/// the daemon's [`ScoreTable`], `hot` the daemon's hot list (mutated: entries
/// are re-validated, refreshed, or dropped), `query` carries `top_k` /
/// `max_tokens`, and `now` is the caller's clock — pass the same instant used
/// for every other time-sensitive read in the recall (hot-list re-validation
/// and reservations).
#[allow(clippy::too_many_arguments)] // pipeline deps; bundled into the recall entry at Wave D
pub fn assemble<F>(
    graph: &Graph,
    expanded: &ExpandedSet,
    phase1: &[Scored<NodeId>],
    scores: &ScoreTable,
    hot: &mut HotList,
    query: &RecallQuery,
    weights: RecallWeights,
    now: DateTime<Utc>,
    token_fn: F,
) -> RecallResult
where
    F: Fn(&str) -> usize,
{
    let (w_daemon, w_query) = (sane_weight(weights.w_daemon), sane_weight(weights.w_query));
    let relevance: HashMap<NodeId, f64> = phase1.iter().map(|s| (s.item, s.score)).collect();
    let daemon: HashMap<NodeId, f64> = scores.ranked.iter().map(|s| (s.item, s.score)).collect();

    // Score every member independently; sort by final score desc, id asc.
    let mut members: Vec<Scored<NodeId>> = expanded
        .required
        .iter()
        .chain(expanded.siblings.iter())
        .cloned()
        .collect();
    for s in &mut members {
        let d = daemon.get(&s.item).copied().unwrap_or(0.0);
        let r = relevance.get(&s.item).copied().unwrap_or(0.0);
        s.score = d * w_daemon + r * w_query;
    }
    members.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.item.0.cmp(&b.item.0))
    });

    // Force-include: re-validate every hot expanded member at `now`, capture
    // the surviving (freshly rebuilt) payloads for rendering.
    let mut hot_payloads: HashMap<NodeId, Vec<HotListPayload>> = HashMap::new();
    let hot_ids: HashSet<NodeId> = expanded
        .required
        .iter()
        .chain(expanded.siblings.iter())
        .map(|s| s.item)
        .collect();
    for &id in &hot_ids {
        if hot.contains(id) && hot.revalidate(graph, id, now) {
            let payloads: Vec<HotListPayload> = hot
                .iter()
                .filter(|e| e.node() == id)
                .map(|e| e.payload().clone())
                .collect();
            if !payloads.is_empty() {
                hot_payloads.insert(id, payloads);
            }
        }
    }

    // top_k normal members (counted by VALID emitted hits — a graph-missing
    // member such as a stale durable-vector id must not consume a top_k slot,
    // GPT5.6sol P2-5), plus every force-included hot member, in score order.
    // Blast radii for every canonical hit, computed ONCE (P2-7).
    let radii = format::blast_radii(graph);
    let mut hits: Vec<RecallHit> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut hit_blocks: Vec<String> = Vec::new(); // block per emitted hit, score order
    let mut emitted = 0usize; // valid non-forced hits accepted toward top_k
    for s in members {
        let forced = hot_payloads.contains_key(&s.item);
        let Some(Node::Concept(c)) = graph.node(s.item) else {
            continue; // graph-missing (e.g. stale vector id): skip, no slot
        };
        if !forced && emitted >= query.top_k {
            continue; // keep scanning: a force-included hot member may sort lower
        }

        let canonical = c.canonization_status == CanonizationStatus::Canonical;
        let hit = RecallHit {
            node_id: c.id,
            content: c.content.clone(),
            concept_type: Some(c.concept_type),
            score: s.score,
            is_canonical: canonical,
            blast_radius: if canonical {
                Some(radii.get(&c.id).copied().unwrap_or(0))
            } else {
                None
            },
        };

        // Warning lines for this hit: ⚑ (canonical), hot-list conditions,
        // then the active reservation (soft lock).
        let mut lines: Vec<String> = Vec::new();
        if canonical {
            lines.push(format::blast_radius_warning(
                hit.blast_radius.unwrap_or_default(),
            ));
        }
        if let Some(payloads) = hot_payloads.get(&c.id) {
            for p in payloads {
                lines.push(format::hot_warning(p));
            }
        }
        if let Some(r) = active_reservation(graph, c.id, now) {
            lines.push(format::reservation_warning(r));
        }

        // Warning lines reflect the included hit set, independent of the token
        // budget (see module docs); a block truncated from the context still
        // reports its conditions.
        let block = format::render_block(&hit, &lines);
        warnings.extend(lines);
        hit_blocks.push(block);
        if !forced {
            emitted += 1;
        }
        hits.push(hit);
    }

    // Context: ranked-prefix over the hit blocks in score order, stopping at
    // the first block that does not fit. The measured token count is of the
    // ACTUAL joined context (separators `\n\n` included), so the rendered
    // output is within budget; a lower-ranked block never follows a skipped
    // one (GPT5.6sol P1-4). Checked arithmetic keeps overflow a no-op stop.
    let mut blocks: Vec<String> = Vec::new();
    for block in hit_blocks {
        let mut provisional = blocks.join("\n\n");
        if !provisional.is_empty() {
            provisional.push('\n');
            provisional.push('\n');
        }
        provisional.push_str(&block);
        let tokens = token_fn(&provisional);
        if tokens.checked_add(1).is_none() || tokens > query.max_tokens {
            break;
        }
        blocks.push(block);
    }

    RecallResult {
        hits,
        context: format::render_context(&blocks),
        warnings,
    }
}



/// Finite, non-negative weight, else `0.0` (mirrors ALGO-10 sanitization).
fn sane_weight(w: f64) -> f64 {
    if w.is_finite() && w >= 0.0 {
        w
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    use crate::daemon::hotlist::{Condition, HotListEntry};
    use crate::types::{AgentId, Concept, ConceptType, Interaction, Reservation, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    /// Concept ids (pair 2, mirroring the candidates.rs test convention).
    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, u))
    }

    /// Interaction ids (pair 1): disjoint from concept ids.
    fn iid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, u))
    }

    fn interaction(id: u64) -> Interaction {
        Interaction {
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(0),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: uid(id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    /// Graph with concepts c1..=cn and one interaction.
    fn graph_with(n: u64) -> Graph {
        let mut g = Graph::new(sid());
        let i1 = interaction(1);
        g.insert_interaction(i1.clone()).unwrap();
        for id in 1..=n {
            g.insert_concept(concept(id, i1.id, &format!("concept {id}")), i1.id)
                .unwrap();
        }
        g
    }

    fn ids_of(result: &RecallResult) -> Vec<NodeId> {
        result.hits.iter().map(|h| h.node_id).collect()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn query(top_k: usize, max_tokens: usize) -> RecallQuery {
        RecallQuery {
            query: "irrelevant".into(),
            top_k,
            max_tokens,
            traversal_depth: 2,
        }
    }

    // -----------------------------------------------------------------------
    // Scoring
    // -----------------------------------------------------------------------

    #[test]
    fn final_score_mixes_daemon_and_relevance_with_planted_weights() {
        let g = graph_with(6);
        // Expanded set: c1/c2 are phase-1 members; c3 is BFS-reached (no
        // phase-1 evidence); c4/c5/c6 are chunk siblings.
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: vec![
                Scored::new(uid(4), 0.0),
                Scored::new(uid(5), 0.0),
                Scored::new(uid(6), 0.0),
            ],
        };
        let phase1 = vec![
            Scored::new(uid(1), 1.0),
            Scored::new(uid(2), 0.5),
            Scored::new(uid(5), 0.2),
        ];
        // c2 is deliberately MISSING from the daemon table (-> 0.0); c6 has a
        // daemon score but no phase-1 evidence (-> relevance 0.0).
        let scores = ScoreTable {
            epoch: 7,
            ranked: vec![
                Scored::new(uid(1), 0.8),
                Scored::new(uid(3), 0.2),
                Scored::new(uid(4), 0.4),
                Scored::new(uid(5), 0.6),
                Scored::new(uid(6), 0.6),
            ],
        };
        let weights = RecallWeights {
            w_daemon: 0.25,
            w_query: 0.75,
        };

        let mut hot = HotList::new();
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &mut hot,
            &query(10, 10_000),
            weights,
            ts(0),
            default_token_count,
        );

        // c1: 0.8×0.25 + 1.0×0.75 = 0.95
        // c2: 0.0×0.25 + 0.5×0.75 = 0.375 (missing daemon score -> 0.0)
        // c3: 0.2×0.25 + 0.0×0.75 = 0.05  (BFS member, no relevance)
        // c4: 0.4×0.25 + 0.0×0.75 = 0.10  (sibling, no relevance)
        // c5: 0.6×0.25 + 0.2×0.75 = 0.30
        // c6: 0.6×0.25 + 0.0×0.75 = 0.15
        let want_order = vec![uid(1), uid(2), uid(5), uid(6), uid(4), uid(3)];
        assert_eq!(ids_of(&result), want_order, "score desc, ties by id asc");
        let score = |id: NodeId| {
            result
                .hits
                .iter()
                .find(|h| h.node_id == id)
                .expect("hit present")
                .score
        };
        assert!(approx(score(uid(1)), 0.95));
        assert!(approx(score(uid(2)), 0.375));
        assert!(approx(score(uid(5)), 0.30));
        assert!(approx(score(uid(6)), 0.15));
        assert!(approx(score(uid(4)), 0.10));
        assert!(approx(score(uid(3)), 0.05));
        // The id-asc tie-break is exercised in the dedicated test below.
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn final_score_ties_break_by_node_id_ascending() {
        let g = graph_with(2);
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(2), 0.0), Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        // Both nodes: daemon 0.8, no relevance -> identical finals regardless
        // of weights; input order is deliberately reversed (c2 first).
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(2), 0.8), Scored::new(uid(1), 0.8)],
        };
        let mut hot = HotList::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(10, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(ids_of(&result), vec![uid(1), uid(2)], "ties by id asc");
    }

    #[test]
    fn non_finite_and_negative_weights_sanitize_to_zero() {
        let g = graph_with(1);
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        let phase1 = vec![Scored::new(uid(1), 1.0)];
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0)],
        };
        let mut hot = HotList::new();
        // NaN and negative weights must not poison the final score (ALGO-10).
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &mut hot,
            &query(10, 10_000),
            RecallWeights {
                w_daemon: f64::NAN,
                w_query: -1.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].score, 0.0);
    }

    // -----------------------------------------------------------------------
    // Hot-list force-include
    // -----------------------------------------------------------------------

    /// Conflict predicate shaped like T4.3's: the recency window is derived
    /// from the caller's `now`, so a lapsed window evicts the entry and a
    /// live one rebuilds `seconds_ago` at read time (XP-3).
    fn conflict_entry(
        node: NodeId,
        writer: AgentId,
        agents: Vec<AgentId>,
        write_at: DateTime<Utc>,
        window_secs: i64,
    ) -> HotListEntry {
        HotListEntry::new(
            node,
            Condition::Conflict,
            HotListPayload::Conflict {
                agents: agents.clone(),
                writer: writer.clone(),
                seconds_ago: 999, // stale sentinel: must be rebuilt before render
            },
            move |_, now| {
                let secs = (now - write_at).num_seconds();
                if (0..=window_secs).contains(&secs) {
                    Some(HotListPayload::Conflict {
                        agents: agents.clone(),
                        writer: writer.clone(),
                        seconds_ago: secs as u64,
                    })
                } else {
                    None
                }
            },
        )
    }

    #[test]
    fn hot_force_include_keeps_live_and_drops_lapsed() {
        let g = graph_with(3);
        // Daemon-only scores -> order c3 > c2 > c1; top_k=1 keeps only c3.
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(3), 1.0),
                Scored::new(uid(2), 0.5),
                Scored::new(uid(1), 0.1),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        let now = ts(60);
        let writer = AgentId::from("agent-a");
        let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
        let mut hot = HotList::new();
        // c1: live (write 11s before now, 30s window) -> force-included.
        let _ = hot.insert(conflict_entry(
            uid(1),
            writer.clone(),
            agents.clone(),
            now - chrono::Duration::seconds(11),
            30,
        ));
        // c2: lapsed (write 31s before now) -> dropped by re-validation.
        let _ = hot.insert(conflict_entry(
            uid(2),
            writer,
            agents,
            now - chrono::Duration::seconds(31),
            30,
        ));

        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(1, 10_000),
            RecallWeights::default(),
            now,
            default_token_count,
        );

        // c3 (rank 1) + force-included c1; c2 is neither in top_k nor hot.
        assert_eq!(ids_of(&result), vec![uid(3), uid(1)]);
        assert_eq!(
            result.warnings,
            vec!["Agent A wrote to it 11 seconds ago".to_string()],
            "rebuilt payload, not the stale 999 sentinel"
        );
        assert!(
            result.context.contains("concept 1"),
            "force-included block rendered"
        );
        assert!(!result.context.contains("concept 2"), "lapsed entry absent");
        // The lapsed entry was dropped from the list; the live one persists
        // with its read-time payload.
        assert!(hot.contains(uid(1)));
        assert!(!hot.contains(uid(2)));
    }

    // -----------------------------------------------------------------------
    // Assembly: top_k, max_tokens, no split blocks
    // -----------------------------------------------------------------------

    #[test]
    fn top_k_and_max_tokens_drop_whole_lowest_blocks() {
        let g = graph_with(4);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.8),
                Scored::new(uid(3), 0.6),
                Scored::new(uid(4), 0.4),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
                Scored::new(uid(4), 0.0),
            ],
            siblings: Vec::new(),
        };
        let mut hot = HotList::new();

        // top_k=3: c4 is excluded from hits entirely.
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(3, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(ids_of(&result), vec![uid(1), uid(2), uid(3)]);
        assert_eq!(result.hits.len(), 3, "top_k respected");

        // Rebuild the rendered blocks the way assemble should, then find the
        // largest score-ordered prefix that fits the budget: the context must
        // equal that prefix (whole blocks only, highest-scoring kept).
        let blocks: Vec<String> = result
            .hits
            .iter()
            .map(|h| crate::recall::format::render_block(h, &[]))
            .collect();
        let mut budget = 0usize;
        let mut kept = 0usize;
        for b in &blocks {
            if budget + default_token_count(b) <= 200 {
                budget += default_token_count(b);
                kept += 1;
            } else {
                break;
            }
        }
        let expected = blocks[..kept].join("\n\n");
        let result2 = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(3, 200),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(result2.context, expected, "longest whole-block prefix");
        assert_eq!(result2.hits.len(), 3, "truncation never removes hits");
        assert!(
            !result2.context.contains("concept 4"),
            "c4 was cut by top_k before rendering"
        );
    }

    #[test]
    fn custom_token_fn_drives_truncation() {
        let g = graph_with(3);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.8),
                Scored::new(uid(3), 0.6),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        let mut hot = HotList::new();

        // token_fn = byte length; budget = exactly block 1's bytes -> only
        // the highest-scoring block survives (whole-block rule).
        let hit1 = RecallHit {
            node_id: uid(1),
            content: "concept 1".into(),
            concept_type: Some(ConceptType::Entity),
            score: 1.0 * 0.5,
            is_canonical: false,
            blast_radius: None,
        };
        let block1 = crate::recall::format::render_block(&hit1, &[]);
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(10, block1.len()),
            RecallWeights::default(),
            ts(0),
            |s| s.len(),
        );
        assert_eq!(result.hits.len(), 3, "hits unaffected by token budget");
        assert_eq!(result.context, block1, "custom estimator honored");
        assert!(!result.context.contains("concept 2"));
    }

    #[test]
    fn max_tokens_zero_yields_empty_context_but_full_hits() {
        let g = graph_with(2);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0), Scored::new(uid(2), 0.8)],
        };
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0), Scored::new(uid(2), 0.0)],
            siblings: Vec::new(),
        };
        let mut hot = HotList::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &mut hot,
            &query(10, 0),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(result.context, "");
        assert_eq!(result.hits.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Reservations
    // -----------------------------------------------------------------------

    #[test]
    fn reservation_rendered_when_active_and_absent_when_expired() {
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0)],
        };
        let now = ts(60);
        let mut hot = HotList::new();

        let mut run = |expires_at: DateTime<Utc>| {
            let mut g = graph_with(1);
            g.set_reservation(Reservation {
                session_id: sid(),
                node_id: uid(1),
                agent_id: AgentId::from("agent-c"),
                expires_at,
            });
            assemble(
                &g,
                &expanded,
                &[],
                &scores,
                &mut hot,
                &query(10, 10_000),
                RecallWeights::default(),
                now,
                default_token_count,
            )
        };

        let live = run(now + chrono::Duration::seconds(60));
        assert_eq!(
            live.warnings,
            vec!["Reserved by agent-c until 2025-07-08T19:41:00Z".to_string()]
        );
        assert!(
            live.context.contains("Reserved by agent-c"),
            "active line shown"
        );

        let gone = run(now - chrono::Duration::seconds(60));
        assert!(
            gone.warnings.is_empty(),
            "expired reservation renders nothing"
        );
        assert!(!gone.context.contains("Reserved by"));
    }

    // -----------------------------------------------------------------------
    // THE GOLDEN: the demo query against the shipped fixture (spec §13).
    //
    // Scenario construction (deterministic):
    // * graph + inverted index: `fixtures/session-rest-api.json`, the spec §13
    //   demo world in miniature;
    // * phase 1: the real union over that index (`limit = top_k`);
    // * phase 2: the real BFS expansion (`traversal_depth = 2`);
    // * daemon scores: the MERGED daemon scorer (`daemon::score::rescore`)
    //   over the fixture — session-relative, wall-clock-free, so the table is
    //   stable;
    // * hot list: built directly with a T4.3-shaped Conflict predicate on
    //   `user schema` whose recency window derives from the caller's `now`;
    // * `now`: a fixed instant (base + 60 min), so the conflict's rebuilt
    //   `seconds_ago` is exactly 11 at read time.
    // The golden file is the byte-for-byte expected context block.
    #[cfg(feature = "fixtures")]
    #[test]
    fn golden_update_user_schema_demo_context_block() {
        use crate::config::ScoringWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::graph::index::InvertedIndex;
        use crate::recall::candidates::{candidates, Phase1Input};
        use crate::recall::expand::expand;

        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Graph::from_snapshot(snap.clone()).unwrap();
        let index = InvertedIndex::from_snapshot(&snap);

        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };
        let phase1 = candidates(
            &graph,
            &index,
            Phase1Input::default(),
            &query.query,
            query.top_k,
        );
        let expanded = expand(&graph, phase1.clone(), query.traversal_depth);
        let scores = ScoreTable {
            epoch: graph.epoch(),
            ranked: rescore(&graph, &ScoringWeights::default()),
        };

        let now = ts(60);
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let mut hot = HotList::new();
        let _ = hot.insert(conflict_entry(
            us,
            AgentId::from("agent-a"),
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            now - chrono::Duration::seconds(11),
            30,
        ));

        let result = assemble(
            &graph,
            &expanded,
            &phase1,
            &scores,
            &mut hot,
            &query,
            RecallWeights::default(),
            now,
            default_token_count,
        );

        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/recall-context-golden.txt"
        );
        let golden = std::fs::read_to_string(golden_path).expect("golden context fixture present");
        assert_eq!(
            result.context, golden,
            "the demo context block is the product: it must render byte-for-byte"
        );

        // The three demo features, explicitly:
        assert!(
            result.context.contains("user schema [Entity, canonical]"),
            "canonical marker on the demo pillar"
        );
        assert!(
            result
                .context
                .contains("⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution."),
            "load-bearing warning with the graph-computed count"
        );
        assert!(
            result
                .context
                .contains("Agent A wrote to it 11 seconds ago"),
            "conflict line: writer, never the first-listed agent"
        );
        assert_eq!(result.hits[0].node_id, us, "demo pillar ranks first");
        assert!(result.hits[0].is_canonical);
        assert_eq!(result.hits[0].blast_radius, Some(8));
        // The accumulated warnings: the ⚑ line then the conflict line, in hit
        // order (user schema is the only warned hit in this result).
        assert_eq!(
            result.warnings,
            vec![
                "⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.".to_string(),
                "Agent A wrote to it 11 seconds ago".to_string(),
            ]
        );
    }

    // P1-4 (GPT5.6sol): the token budget charges the separator AND stops at the
    // first non-fitting block (ranked-prefix) — a lower-ranked short block must
    // not follow a skipped higher-ranked one. Provable with a tiny token_fn
    // that returns the byte length, and blocks whose sizes straddle a budget.
    #[test]
    fn budget_charges_separators_and_enforces_ranked_prefix() {
        let g = graph_with(3);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.5),
                Scored::new(uid(3), 0.1),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        // token_fn = byte length; block for concept "concept 1" (len ~ "concept 1 [Entity]
        // (score 1.00)" ~ 30) each block ~ >; use a budget that fits block 1 plus the
        // separator but not block 2's separator+block? Instead: pick budget so block1
        // fits alone and block1+sep+block2 does not -> only block1 rendered (ranked-prefix:
        // block2, though it might fit alone, must NOT appear after block1).
        let result = assemble(
            &g, &expanded, &[], &scores, &mut HotList::new(),
            &query(3, 34), RecallWeights::default(), ts(60), byte_len,
        );
        let contexts = result.context.split("\n\n").collect::<Vec<_>>();
        assert_eq!(contexts.len(), 1, "ranked-prefix: only the first block fits");
        assert!(result.context.starts_with("concept 1"), "first block kept");

        // With a large budget everything fits in rank order.
        let all = assemble(
            &g, &expanded, &[], &scores, &mut HotList::new(),
            &query(3, 10_000), RecallWeights::default(), ts(60), byte_len,
        );
        assert_eq!(all.context.split("\n\n").count(), 3);
    }

    // P2-5 (GPT5.6sol): a graph-missing member within top_k (e.g. a stale durable
    // vector id) must NOT consume a top_k slot — the next valid member fills it.
    #[test]
    fn stale_graph_missing_member_does_not_consume_top_k_slot() {
        // expanded.required includes uid(9) which is NOT in the graph (stale
        // vector id), ahead of the valid c1. top_k=1 must yield [c1], not [].
        let g = graph_with(3);
        let scores = ScoreTable { epoch: 0, ranked: vec![] };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(9), 0.9), // graph-missing / stale
                Scored::new(uid(1), 0.1),
            ],
            siblings: Vec::new(),
        };
        let result = assemble(
            &g, &expanded, &[], &scores, &mut HotList::new(),
            &query(1, 10_000), RecallWeights::default(), ts(60), byte_len,
        );
        assert_eq!(ids_of(&result), vec![uid(1)], "valid member fills the slot");
    }

    fn byte_len(s: &str) -> usize {
        s.len()
    }

}