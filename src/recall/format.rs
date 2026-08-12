//! Recall context format (T5.3, spec §8 / v0.6.0 §9.2) — the text the demo
//! video shows on screen.
//!
//! One hit renders as a block of lines, in this order:
//!
//! ```text
//! <content> [<ConceptType>{, canonical}] (score <final>, blast radius <n>)
//! <⚑ load-bearing-pillar line>        (only when the concept is Canonical)
//! <hot-list condition line(s)>        (only for re-validated hot nodes)
//! <reservation line>                  (only while an active reservation holds)
//! ```
//!
//! Blocks are joined with a blank line ([`render_context`]); a block is never
//! split by the token budget (the truncation rule lives in `assemble`).
//!
//! ## The ⚑ line (spec §13, verbatim)
//!
//! ```text
//! ⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.
//! ```
//!
//! The template is reproduced byte-for-byte, including the em dash and the
//! flag glyph; only the count is interpolated. This is the spec-verbatim
//! golden text (repo rule: em dashes are otherwise banned in docs; this one
//! is the exception, exactly as spec §13 quotes it).
//!
//! ## Blast radius (spec §4.1 contract + errata 2026-08-11)
//!
//! [`blast_radius`] computes the Stage-3 dependent count over the in-RAM
//! graph, mirroring `GraphStore::blast_radius` (the three-way MemoryStore /
//! SQLite / Cockroach agreement, T3.6): count concepts `c != node` that have
//! at least one inbound structural edge from `node` and no inbound structural
//! edge from any other concept. Structural means
//! `Dependency` / `Causal` / `Hierarchical` only; the mandatory provenance
//! `Derives` (interaction → concept) and `Temporal` edges never count as
//! "another inbound source", or Stage-3 blast radius would be zero for every
//! legal graph (errata). The definition is 1-hop, no recursion.
//!
//! No edge-age cutoff applies here: recall renders over the current graph
//! (the store query's `min_edge_age` is a T3.x adapter concern), and the
//! committed fixtures put every edge in the past by convention.
//!
//! **Fixture note (shipped numbers):** on `fixtures/session-rest-api.json`
//! the demo pillar "user schema" computes **8** dependents, pinned by
//! `src/fixtures.rs` (`blast_radius: Some(8)`), the Cockroach anchor test,
//! and `scripts/gen-fixtures.py` ("blast_radius = 8 > 5"). Spec §13 and
//! `dev-diary/PHASE-8-surface.md` narrate the live demo with **9**; that
//! number belongs to the T8.4 demo-scenario graph, which must plant a 9th
//! dependent. The format renders whatever the graph computes.
//!
//! ## The conflict line
//!
//! ```text
//! Agent A wrote to it 11 seconds ago
//! ```
//!
//! Subject is the payload's `writer` (ALGO-2) — never guessed from the
//! `agents` list, which on the shipped fixture would pick the wrong agent
//! (the newest write is agent-b's). [`agent_display`] maps the fixture's id
//! convention (`agent-a` → `A`). The age renders as a numeral, matching the
//! events channel's `"{}s ago"` convention and PHASE-8's "11-seconds-ago
//! conflict line"; spec §13's "eleven seconds ago" is demo narration, not
//! quoted screen text (unlike the ⚑ line above).

use std::collections::HashSet;

use crate::daemon::hotlist::HotListPayload;
use crate::graph::Graph;
use crate::types::{AgentId, EdgeType, NodeId, RecallHit, Reservation};

/// The structural edge types that carry dependency for Stage-3 blast radius
/// (spec §4.1 errata: `Derives` provenance and `Temporal` never count).
pub const STRUCTURAL_EDGE_TYPES: [EdgeType; 3] = [
    EdgeType::Dependency,
    EdgeType::Causal,
    EdgeType::Hierarchical,
];

/// Stage-3 dependent count for `node` (spec §4.1 contract; see module docs).
///
/// Deterministic for a given graph: the count is a pure set predicate over
/// concepts and structural edges, independent of iteration order. Pure and
/// lock-safe like the rest of the recall pipeline.
pub fn blast_radius(graph: &Graph, node: NodeId) -> u64 {
    let concept_ids: HashSet<NodeId> = graph.concepts().map(|c| c.id).collect();
    let mut count = 0u64;
    for c in graph.concepts() {
        if c.id == node {
            continue;
        }
        let mut from_node = false;
        let mut from_other = false;
        for e in graph.edges() {
            if e.target != c.id {
                continue;
            }
            if !STRUCTURAL_EDGE_TYPES.contains(&e.edge_type) || !concept_ids.contains(&e.source) {
                continue;
            }
            if e.source == node {
                from_node = true;
            } else {
                from_other = true;
            }
        }
        if from_node && !from_other {
            count += 1;
        }
    }
    count
}

/// The `[Entity, canonical]` label: concept content + type, plus the
/// canonical marker exactly when the concept is Canonical (spec §10: only
/// canonical nodes are marked, and always marked).
pub fn concept_label(hit: &RecallHit) -> String {
    let ty = hit
        .concept_type
        .map(|t| format!("{t:?}"))
        .unwrap_or_default();
    let marker = if hit.is_canonical { ", canonical" } else { "" };
    format!("{} [{ty}{marker}]", hit.content)
}

/// The spec §13 load-bearing-pillar warning, verbatim except for the count.
pub fn blast_radius_warning(count: u64) -> String {
    format!("⚑ Load-bearing pillar — {count} nodes depend on this. Modify with caution.")
}

/// Human-readable agent name: strips the fixture's `agent-` prefix and
/// capitalizes (`agent-a` → `A`); any other id is used as-is (capitalized).
pub fn agent_display(id: &AgentId) -> String {
    let rest = id.as_str().strip_prefix("agent-").unwrap_or(id.as_str());
    let mut out = String::with_capacity(rest.len());
    for (i, ch) in rest.chars().enumerate() {
        if i == 0 {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The demo conflict sentence: subject is the payload's `writer` (ALGO-2).
pub fn conflict_warning(writer: &AgentId, seconds_ago: u64) -> String {
    format!(
        "Agent {} wrote to it {} seconds ago",
        agent_display(writer),
        seconds_ago
    )
}

/// One warning line per hot-list condition kind. Conflict renders the §13
/// sentence; the other three render compact, deterministic lines from their
/// payloads (the demo surface is conflict; the others must not be silent).
pub fn hot_warning(payload: &HotListPayload) -> String {
    match payload {
        HotListPayload::Conflict {
            writer,
            seconds_ago,
            ..
        } => conflict_warning(writer, *seconds_ago),
        HotListPayload::HighRisk { reason } => format!("High-risk modification: {reason}"),
        HotListPayload::Drift { hops, root } => {
            format!("Drifted {hops} hops from root goal {root}")
        }
        HotListPayload::Stale { seconds_inactive } => {
            format!("Session inactive {seconds_inactive} seconds")
        }
    }
}

/// The active soft-lock line (spec §10.3.3, T2.7): agent + expiry.
pub fn reservation_warning(r: &Reservation) -> String {
    format!(
        "Reserved by {} until {}",
        r.agent_id,
        r.expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// One hit's full block: the label line (content, type marker, score, blast
/// radius) followed by its warning lines. Warnings are passed in already
/// ordered: ⚑ line, hot-list lines, reservation line.
pub fn render_block(hit: &RecallHit, warnings: &[String]) -> String {
    let meta = match hit.blast_radius {
        Some(radius) => format!("(score {:.2}, blast radius {radius})", hit.score),
        None => format!("(score {:.2})", hit.score),
    };
    let mut lines = vec![format!("{} {meta}", concept_label(hit))];
    lines.extend(warnings.iter().cloned());
    lines.join("\n")
}

/// The full context: rendered blocks in final-score order, separated by one
/// blank line. No trailing newline, so the string is stable as a payload and
/// as golden text.
pub fn render_context(blocks: &[String]) -> String {
    blocks.join("\n\n")
}

/// The built-in token estimator: `ceil(byte_len / 3.5)` — the common
/// "roughly 3.5 bytes per token" heuristic for mixed prose. Pass this to
/// [`crate::recall::assemble::assemble`] (or any `Fn(&str) -> usize`) as the
/// `token_fn`.
pub fn default_token_count(s: &str) -> usize {
    (s.len() as f64 / 3.5).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use crate::types::{CanonizationStatus, Concept, ConceptType, Interaction, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    fn iid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, u))
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

    fn interaction(id: u64, agent: &str) -> Interaction {
        Interaction {
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from(agent),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(0),
        }
    }

    /// x -> a dependency only; x -> b also sourced by y; provenance edges and
    /// an interaction-sourced structural edge must all be ignored.
    fn blast_graph() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, "agent-a");
        let i2 = {
            let mut i = interaction(2, "agent-b");
            i.previous_id = Some(i1.id);
            i
        };
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        let x = uid(10);
        let a = uid(11);
        let b = uid(12);
        let y = uid(13);
        let c = uid(14);
        let d = uid(15);
        for (id, content) in [
            (10u64, "x"),
            (11, "a"),
            (12, "b"),
            (13, "y"),
            (14, "c"),
            (15, "d"),
        ] {
            g.insert_concept(concept(id, i1.id, content), i1.id)
                .unwrap();
        }
        let dep = |s: NodeId, t: NodeId, m: i64| crate::types::Edge {
            id: uid(100 + m as u64),
            session_id: sid(),
            source: s,
            target: t,
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(m),
            last_reinforced: ts(m),
        };
        // a depends only on x: counted.
        g.upsert_edge(dep(x, a, 1)).unwrap();
        // b depends on x AND y: not an orphan of x.
        g.upsert_edge(dep(x, b, 2)).unwrap();
        g.upsert_edge(dep(y, b, 3)).unwrap();
        // c -> x is inbound to x: irrelevant to x's blast radius.
        g.upsert_edge(dep(c, x, 4)).unwrap();
        // d depends only on y: y's exclusive orphan.
        g.upsert_edge(dep(y, d, 8)).unwrap();
        // NOTE: an interaction-sourced structural edge cannot exist in a legal
        // graph (spec §5: Dependency/Causal/Hierarchical must connect Concept
        // to Concept; the write gate rejects it), so `blast_radius`'s
        // concept-source filter is defensive belt-and-suspenders, not a
        // reachable case.
        // Provenance Derives (i1 -> a) and a Temporal edge: must not count.
        g.upsert_edge(crate::types::Edge {
            id: uid(106),
            session_id: sid(),
            source: i1.id,
            target: a,
            edge_type: EdgeType::Derives,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(6),
            last_reinforced: ts(6),
        })
        .unwrap();
        g.upsert_edge(crate::types::Edge {
            id: uid(107),
            session_id: sid(),
            source: i2.id,
            target: i1.id,
            edge_type: EdgeType::Temporal,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(7),
            last_reinforced: ts(7),
        })
        .unwrap();
        g
    }

    #[test]
    fn blast_radius_counts_only_exclusive_concept_orphans() {
        let g = blast_graph();
        assert_eq!(blast_radius(&g, uid(10)), 1, "only `a` is exclusively x's");
        assert_eq!(
            blast_radius(&g, uid(13)),
            1,
            "`b` keeps its x-edge, so `d` is y's only exclusive orphan"
        );
        assert_eq!(blast_radius(&g, uid(11)), 0, "`a` has no dependents");
        assert_eq!(blast_radius(&g, uid(14)), 1, "`x` depends only on c");
    }

    #[test]
    fn blast_radius_warning_line_is_byte_exact() {
        // Spec §13 verbatim, em dash included; only the count interpolates.
        assert_eq!(
            blast_radius_warning(9),
            "⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution."
        );
        assert_eq!(
            blast_radius_warning(8),
            "⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution."
        );
    }

    #[test]
    fn concept_label_marks_only_canonical() {
        let hit = |canonical: bool| RecallHit {
            node_id: uid(1),
            content: "user schema".into(),
            concept_type: Some(ConceptType::Entity),
            score: 0.7,
            is_canonical: canonical,
            blast_radius: None,
        };
        assert_eq!(concept_label(&hit(true)), "user schema [Entity, canonical]");
        assert_eq!(concept_label(&hit(false)), "user schema [Entity]");
    }

    #[test]
    fn conflict_warning_renders_writer_not_first_agent() {
        // ALGO-2: the naive first-listed agent is wrong on the shipped
        // fixture; the subject is the payload's writer, never `agents[0]`.
        assert_eq!(
            conflict_warning(&AgentId::from("agent-b"), 11),
            "Agent B wrote to it 11 seconds ago"
        );
        assert_eq!(
            conflict_warning(&AgentId::from("agent-a"), 11),
            "Agent A wrote to it 11 seconds ago"
        );
        // Non-prefixed ids render as-is (capitalized).
        assert_eq!(
            conflict_warning(&AgentId::from("alice"), 5),
            "Agent Alice wrote to it 5 seconds ago"
        );
    }

    #[test]
    fn hot_warning_covers_all_condition_kinds() {
        use crate::types::AgentId;
        assert_eq!(
            hot_warning(&HotListPayload::Conflict {
                agents: vec![AgentId::from("agent-a")],
                writer: AgentId::from("agent-a"),
                seconds_ago: 11,
            }),
            "Agent A wrote to it 11 seconds ago"
        );
        assert_eq!(
            hot_warning(&HotListPayload::HighRisk {
                reason: "breaking migration".into()
            }),
            "High-risk modification: breaking migration"
        );
        assert_eq!(
            hot_warning(&HotListPayload::Drift {
                hops: 3,
                root: uid(7),
            }),
            format!("Drifted 3 hops from root goal {}", uid(7))
        );
        assert_eq!(
            hot_warning(&HotListPayload::Stale {
                seconds_inactive: 300
            }),
            "Session inactive 300 seconds"
        );
    }

    #[test]
    fn reservation_warning_is_deterministic_rfc3339() {
        let r = Reservation {
            session_id: sid(),
            node_id: uid(1),
            agent_id: AgentId::from("agent-c"),
            expires_at: ts(5),
        };
        assert_eq!(
            reservation_warning(&r),
            "Reserved by agent-c until 2025-07-08T18:45:00Z"
        );
    }

    #[test]
    fn render_block_and_context_layout() {
        let hit = RecallHit {
            node_id: uid(1),
            content: "user schema".into(),
            concept_type: Some(ConceptType::Entity),
            score: 0.7182,
            is_canonical: true,
            blast_radius: Some(8),
        };
        let warnings = vec![
            blast_radius_warning(8),
            conflict_warning(&AgentId::from("agent-a"), 11),
        ];
        let block = render_block(&hit, &warnings);
        assert_eq!(
            block,
            "user schema [Entity, canonical] (score 0.72, blast radius 8)\n\
             ⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.\n\
             Agent A wrote to it 11 seconds ago"
        );
        assert_eq!(render_context(&[block]), "user schema [Entity, canonical] (score 0.72, blast radius 8)\n⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.\nAgent A wrote to it 11 seconds ago");
        assert_eq!(render_context(&[]), "");
        assert_eq!(render_context(&["a".into(), "b".into()]), "a\n\nb");
    }

    #[test]
    fn default_token_count_is_ceil_bytes_over_3_5() {
        assert_eq!(default_token_count(""), 0);
        assert_eq!(default_token_count("abc"), 1); // ceil(3/3.5)
        assert_eq!(default_token_count("abcdefg"), 2); // ceil(7/3.5)
        assert_eq!(default_token_count("aaaaaaaaaaaaaaaa"), 5); // ceil(16/3.5)
    }

    /// The shipped demo pillar computes its dependent count from the graph
    /// (spec §4.1 contract), not from the fixture's stored `blast_radius`
    /// field. Both agree on the fixture: 8.
    #[cfg(feature = "fixtures")]
    #[test]
    fn fixture_schema_user_blast_radius_computes_eight() {
        use crate::fixtures;
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let g = Graph::from_snapshot(snap).expect("fixture loads");
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        assert_eq!(
            concept_label(&RecallHit {
                node_id: us,
                content: "user schema".into(),
                concept_type: Some(ConceptType::Entity),
                score: 0.0,
                is_canonical: true,
                blast_radius: None,
            }),
            "user schema [Entity, canonical]"
        );
        assert_eq!(
            blast_radius(&g, us),
            8,
            "user schema: 8 exclusive orphans (D1..D8). Spec §13's demo narration says 9; that number belongs to the T8.4 live demo graph (see module docs)."
        );
    }
}
