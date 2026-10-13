//! #19(b) over MCP: `against` on a `lambo_derive` concept and on
//! `lambo_record_action`. The cross-session refusal is in `layered`.

use super::*;
use crate::recall::format;
use crate::types::EdgeType;

const BELIEF: &str = "brand y chargers are safe to leave plugged in";

async fn belief(s: &LamboServer) -> NodeId {
    let ids = derive_created(
        s,
        "agent-a",
        json!([{"content": BELIEF, "concept_type": "logic"}]),
    )
    .await;
    NodeId(ids[0].parse().expect("uuid"))
}

fn node(s: &LamboServer, content: &str) -> NodeId {
    s.mem
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == content)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
        .id
}

/// A derive's `against` and an action's land as `Contradicts` edges, newer
/// to older; the target gains no dependent (inspect's list is unchanged)
/// and nothing is created for a reference.
#[tokio::test]
async fn a_contradicting_derive_and_action_land_on_their_targets() {
    let s = server("mcp-against").await;
    let belief = belief(&s).await;
    let newer = "brand y chargers overheat when left plugged in";
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": newer,
            "concept_type": "observation",
            "against": [format::short_id(belief)],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    let concepts = s.mem.graph().read().concepts().count();
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "refunded: the charger overheated",
               "against": [belief.0.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        let n = g.concepts().find(|c| c.content == newer).unwrap().id;
        let a = g
            .concepts()
            .find(|c| c.content == "refunded: the charger overheated")
            .unwrap()
            .id;
        assert!(g.edge_between(n, belief, EdgeType::Contradicts).is_some());
        assert!(g.edge_between(a, belief, EdgeType::Contradicts).is_some());
        assert!(g
            .edges()
            .all(|e| e.target != belief || e.edge_type != EdgeType::Dependency));
        assert_eq!(g.concepts().count(), concepts + 1, "only the action");
        // Opposition is not dependence: no blast radius.
        assert_eq!(format::blast_radius(&g, belief), 0);
    }
    s.mem.close().await.expect("close");
}

/// Every refusal arrives before the ack, as an error naming `against`,
/// and writes nothing: missing, ambiguous by content shared with a short
/// id, an interaction's id, the concept itself, and on an action a
/// missing target (never created, unlike `depends_on` text).
#[tokio::test]
async fn a_bad_against_is_refused_before_the_ack_with_its_reason() {
    let s = server("mcp-against-refuse").await;
    let belief = belief(&s).await;
    let before = s.mem.graph().read().snapshot();
    let interaction = s
        .mem
        .graph()
        .read()
        .interactions()
        .next()
        .expect("an interaction")
        .id;
    for (content, reference, says) in [
        (
            "a newer claim",
            "chargers are fine".to_string(),
            "names no concept in this session",
        ),
        (
            "a newer claim",
            interaction.0.to_string(),
            "is the id of an interaction",
        ),
        (
            BELIEF,
            format::short_id(belief),
            "names the contradicting concept itself",
        ),
    ] {
        let out = call_raw(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": content, "concept_type": "logic", "against": [reference],
            }]}),
        )
        .await;
        assert_eq!(out.is_error, Some(true));
        let text = text_of(&out);
        assert!(
            text.starts_with("lambo_derive: reference refused: against \""),
            "{text}"
        );
        assert!(text.contains(says), "{text}");
        assert!(text.ends_with("nothing was written"), "{text}");
    }
    let out = call_raw(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "ran it", "against": ["chargers are fine"]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).starts_with("lambo_record_action: reference refused: against \"chargers are fine\" on \"ran it\" names no concept"),
        "{}",
        text_of(&out)
    );
    assert_eq!(s.mem.graph().read().snapshot(), before);
    s.mem.close().await.expect("close");
}

/// The surface limits: 8 per concept (or action), 32 across a derive,
/// counted apart from `depends_on`; empty entries are bad parameters.
#[tokio::test]
async fn the_against_limits_are_bad_params() {
    let s = server("mcp-against-limits").await;
    let nine: Vec<String> = (0..9).map(|n| format!("r{n}")).collect();
    for (tool, args, says) in [
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [
                {"content": "a", "concept_type": "logic", "against": nine}]}),
            "a concept's against may list at most 8",
        ),
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [
                {"content": "a", "concept_type": "logic", "against": [""]}]}),
            "concept.against entry",
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "ran it", "against": nine}),
            "an action's against may list at most 8",
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "ran it", "against": [" "]}),
            "against entry",
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true));
        assert!(text_of(&out).contains(says), "{}", text_of(&out));
    }
    let five: Vec<serde_json::Value> = (0..5)
        .map(|n| {
            json!({"content": format!("c{n}"), "concept_type": "logic",
                   "depends_on": ["a","b","c","d","e","f","g"],
                   "against": ["a","b","c","d","e","f","g"]})
        })
        .collect();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": five}),
    )
    .await;
    // 35 citations is over first; then 35 contradictions on their own.
    assert!(
        text_of(&out).contains("depends_on may list at most 32"),
        "{}",
        text_of(&out)
    );
    let five: Vec<serde_json::Value> = (0..5)
        .map(|n| {
            json!({"content": format!("c{n}"), "concept_type": "logic",
                   "against": ["a","b","c","d","e","f","g"]})
        })
        .collect();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": five}),
    )
    .await;
    assert!(
        text_of(&out).contains("against may list at most 32"),
        "{}",
        text_of(&out)
    );
    s.mem.close().await.expect("close");
}

/// Two concepts may contradict each other over the wire (no acyclicity),
/// and re-sending a contradiction reinforces its one edge.
#[tokio::test]
async fn mutual_and_resent_contradictions_over_the_wire() {
    let s = server("mcp-against-mutual").await;
    let belief = belief(&s).await;
    let newer = "brand y chargers overheat when left plugged in";
    for _ in 0..2 {
        let out = call(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": newer, "concept_type": "logic",
                "against": [format::short_id(belief)],
            }]}),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    }
    let n = node(&s, newer);
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": BELIEF, "concept_type": "logic",
            "against": [format::short_id(n)],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        assert_eq!(
            g.edge_between(n, belief, EdgeType::Contradicts)
                .expect("newer -> older")
                .reinforcements,
            2
        );
        assert!(g.edge_between(belief, n, EdgeType::Contradicts).is_some());
        assert_eq!(
            g.edges()
                .filter(|e| e.edge_type == EdgeType::Contradicts)
                .count(),
            2
        );
    }
    s.mem.close().await.expect("close");
}

/// #19(b) review M2 over the wire: one concept, or one action, that both
/// cites and contradicts the same concept is refused before the ack, and
/// nothing is written.
#[tokio::test]
async fn citing_and_contradicting_one_concept_in_one_call_is_refused() {
    let s = server("mcp-against-both").await;
    let belief = belief(&s).await;
    let before = s.mem.graph().read().snapshot();
    let short = format::short_id(belief);
    for (tool, args) in [
        (
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": "a newer claim", "concept_type": "logic",
                "depends_on": [short.clone()], "against": [BELIEF],
            }]}),
        ),
        (
            "lambo_record_action",
            json!({"agent_id": "agent-a", "action": "acted on it",
                   "depends_on": [BELIEF], "against": [short.clone()]}),
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true), "{tool}");
        let text = text_of(&out);
        assert!(
            text.contains("reference refused: against")
                && text.contains("also cites in depends_on"),
            "{tool}: {text}"
        );
    }
    assert_eq!(s.mem.graph().read().snapshot(), before);
    s.mem.close().await.expect("close");
}
