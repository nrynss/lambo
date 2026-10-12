//! #36 PR 4 over MCP: `lambo_derive`'s `depends_on` and `name`, and
//! `lambo_record_action`'s id entries. The cross-session refusal is in
//! `layered`.

use super::*;
use crate::recall::format;
use crate::types::EdgeType;

const RULING: &str = "the writer lease stays with its holder until the holder releases it";

async fn ruling(s: &LamboServer) -> NodeId {
    let ids = derive_created(
        s,
        "agent-a",
        json!([{"content": RULING, "concept_type": "logic"}]),
    )
    .await;
    NodeId(ids[0].parse().expect("uuid"))
}

#[tokio::test]
async fn a_cited_derive_and_a_named_action_land_on_the_ruling() {
    let s = server("mcp-cite").await;
    let ruling = ruling(&s).await;
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": "flush retries keep the lease",
            "concept_type": "logic",
            "depends_on": [format::short_id(ruling)],
            "name": "retry lease rule",
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "retried a flush",
               "depends_on": ["retry lease rule", ruling.0.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        let id = |content: &str| g.concepts().find(|c| c.content == content).unwrap().id;
        let decision = id("flush retries keep the lease");
        let action = id("retried a flush");
        assert!(g
            .edge_between(decision, ruling, EdgeType::Dependency)
            .is_some());
        assert!(g
            .edge_between(action, decision, EdgeType::Dependency)
            .is_some());
        assert!(g
            .edge_between(action, ruling, EdgeType::Dependency)
            .is_some());
        assert!(g.concepts().all(|c| c.content != "retry lease rule"));
    }
    // Inspect names the citing work as dependents.
    let out = call(
        &s,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": format::short_id(ruling)}),
    )
    .await;
    assert!(
        text_of(&out).contains("dependents (2):"),
        "{}",
        text_of(&out)
    );
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn a_bad_reference_is_refused_before_the_ack_with_its_reason() {
    let s = server("mcp-cite-refuse").await;
    ruling(&s).await;
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": "a decision", "concept_type": "logic",
            "depends_on": ["writer lease stays"],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    let text = text_of(&out);
    assert!(
        text.starts_with("lambo_derive: reference refused: depends_on \"writer lease stays\" on \"a decision\" names no concept in this session"),
        "{text}"
    );
    assert!(text.ends_with("nothing was written"), "{text}");
    assert!(out.structured_content.is_none() || !text.contains("receipt"));

    let unknown = NodeId::new().0.to_string();
    let out = call_raw(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "ran it", "depends_on": [unknown]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).contains("reference refused"),
        "{}",
        text_of(&out)
    );
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn the_surface_limits_are_bad_params() {
    let s = server("mcp-cite-limits").await;
    let nine: Vec<String> = (0..9).map(|n| format!("r{n}")).collect();
    for (concepts, says) in [
        (
            json!([{"content": "a", "concept_type": "logic", "depends_on": nine}]),
            "at most 8",
        ),
        (
            json!([{"content": "a", "concept_type": "logic", "depends_on": [""]}]),
            "concept.depends_on entry",
        ),
        (
            json!([{"content": "a", "concept_type": "logic", "name": "n".repeat(257)}]),
            "at most 256 bytes",
        ),
    ] {
        let out = call_raw(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": concepts}),
        )
        .await;
        assert_eq!(out.is_error, Some(true));
        assert!(text_of(&out).contains(says), "{}", text_of(&out));
    }
    let five: Vec<serde_json::Value> = (0..5)
        .map(|n| {
            json!({"content": format!("c{n}"), "concept_type": "logic",
                        "depends_on": ["a","b","c","d","e","f","g"]})
        })
        .collect();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": five}),
    )
    .await;
    assert!(text_of(&out).contains("at most 32"), "{}", text_of(&out));
    s.mem.close().await.expect("close");
}
