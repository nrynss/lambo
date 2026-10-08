//! The `inspect` neighbourhood projection, shared by `lambo inspect` and
//! `lambo_inspect`: a bounded BFS around a resolved focus, rendered as text
//! and as structured JSON. Caps: [`MAX_INSPECT_NODES`] (a total across every
//! hop); the depth bound ([`super::limits::MAX_INSPECT_DEPTH`]) is each
//! caller's door check.

use serde_json::json;

use super::limits::MAX_INSPECT_NODES;
use crate::graph::Graph;
use crate::recall::format;
use crate::types::{CanonizationStatus, EdgeType, Node, NodeId};

/// Render a BFS neighbourhood around `target`. Caller holds the graph read
/// lock; this function never awaits (spec §6.4).
pub(crate) fn render_neighbourhood(
    g: &Graph,
    target: NodeId,
    depth: usize,
) -> (String, serde_json::Value) {
    use std::collections::{HashMap, HashSet};

    let radii = format::blast_radii(g);
    let label = |id: NodeId| -> String {
        match g.node(id) {
            Some(Node::Concept(c)) => {
                let canon = match c.canonization_status {
                    CanonizationStatus::Canonical => ", canonical",
                    CanonizationStatus::Venerable => ", venerable",
                    CanonizationStatus::Candidate => ", candidate",
                    CanonizationStatus::None => "",
                };
                format!("{} [{:?}{}]", c.content, c.concept_type, canon)
            }
            Some(Node::Interaction(i)) => {
                format!("<interaction {}>", i.id.0)
            }
            None => format!("<missing {}>", id.0),
        }
    };

    let mut text = String::new();
    text.push_str(&format!("focus: {}\n", label(target)));
    if let Some(r) = radii.get(&target) {
        text.push_str(&format!("blast radius: {r}\n"));
        if *r > 0 {
            text.push_str(&format!("{}\n", format::blast_radius_warning(*r)));
        }
    }
    if let Some(res) = g.reservation(target) {
        text.push_str(&format!("{}\n", format::reservation_warning(res)));
    }

    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(target);
    let mut frontier = vec![target];
    let mut levels: Vec<serde_json::Value> = Vec::new();
    let mut budget = MAX_INSPECT_NODES;

    for hop in 1..=depth {
        let mut next = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut by_type: HashMap<EdgeType, Vec<String>> = HashMap::new();
        for &node in &frontier {
            for edge in g.incident_edges(node) {
                let other = if edge.source == node {
                    edge.target
                } else {
                    edge.source
                };
                // Budget first, `seen` second: marking a node seen and *then*
                // discovering the budget is spent permanently excludes a
                // neighbour that was never rendered.
                if budget == 0 {
                    break;
                }
                if !seen.insert(other) {
                    continue;
                }
                budget -= 1;
                let dir = if edge.source == node { "->" } else { "<-" };
                by_type
                    .entry(edge.edge_type)
                    .or_default()
                    .push(format!("{dir} {}", label(other)));
                rows.push(json!({
                    "node_id": other.0.to_string(),
                    "label": label(other),
                    "edge_type": format!("{:?}", edge.edge_type),
                    "direction": dir,
                    "weight": edge.weight,
                }));
                next.push(other);
            }
        }
        if rows.is_empty() {
            break;
        }
        text.push_str(&format!("\nhop {hop}:\n"));
        let mut kinds: Vec<_> = by_type.into_iter().collect();
        kinds.sort_by_key(|(k, _)| format!("{k:?}"));
        for (kind, mut entries) in kinds {
            entries.sort();
            text.push_str(&format!("  {kind:?}\n"));
            for e in entries {
                text.push_str(&format!("    {e}\n"));
            }
        }
        levels.push(json!({ "hop": hop, "neighbours": rows }));
        frontier = next;
        if budget == 0 {
            text.push_str(&format!(
                "\n(truncated at {MAX_INSPECT_NODES} neighbours)\n"
            ));
            break;
        }
    }

    let structured = json!({
        "node_id": target.0.to_string(),
        "label": label(target),
        "blast_radius": radii.get(&target).copied().unwrap_or(0),
        "levels": levels,
    });
    (text, structured)
}
