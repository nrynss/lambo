//! #36 PR 4 over MCP: `lambo_derive`'s `depends_on`, and
//! `lambo_record_action`'s id entries. The cross-session refusal is in
//! `layered`.

use super::*;
use crate::recall::format;
use crate::types::EdgeType;

const RULING: &str = "the writer lease stays with its holder until the holder releases it";

#[tokio::test]
async fn durable_names_on_the_wire_resolve_and_conflicts_write_nothing() {
    let s = server("mcp-durable-names").await;
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id":"agent-a", "concepts":[{
            "content":RULING,"concept_type":"logic","name":"Lease café"
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    let target = s
        .mem
        .graph()
        .read()
        .name_target("LEASE cafe\u{301}")
        .unwrap();
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id":"agent-a",
        "action":"verified the named contract","depends_on":["LEASE cafe\u{301}"]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let graph = s.mem.graph().read();
        let action = graph
            .concepts()
            .find(|c| c.content == "verified the named contract")
            .unwrap()
            .id;
        assert!(graph
            .edge_between(action, target, EdgeType::Dependency)
            .is_some());
        assert_eq!(graph.names().count(), 1);
    }
    let before = s.mem.graph().read().snapshot();
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id":"agent-a", "concepts":[{
            "content":"a different fact","concept_type":"logic","name":"lease CAFÉ"
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true), "{}", text_of(&out));
    assert_eq!(s.mem.graph().read().snapshot(), before);
    s.mem.close().await.unwrap();
}

#[tokio::test]
async fn repaired_alias_ids_remain_inspectable_and_refuse_new_citations() {
    let s = server("mcp-retired-alias").await;
    let target = ruling(&s).await;
    let ids = derive_created(
        &s,
        "agent-a",
        json!([{
            "content":"writer lease","concept_type":"entity"
        }]),
    )
    .await;
    let alias = NodeId(ids[0].parse().unwrap());
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id":"agent-a",
        "action":"followed the original alias","depends_on":[alias.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    crate::graph::names::upgrade(&mut s.mem.graph().write(), &[(alias, target)]).unwrap();
    for focus in [alias.to_string(), format::short_id(alias)] {
        let out = call(
            &s,
            "lambo_inspect",
            json!({"agent_id":"agent-a","focus":focus}),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
        assert!(text_of(&out).contains("retired alias"));
        let retired = &out.structured_content.as_ref().unwrap()["retired_alias"];
        assert_eq!(retired["replacement"], json!(target));
        assert_eq!(retired["content"], "writer lease");
        assert!(retired.get("embedding").is_none());
    }
    for reference in [alias.to_string(), format::short_id(alias)] {
        let before = s.mem.graph().read().snapshot();
        let out = call(
            &s,
            "lambo_record_action",
            json!({"agent_id":"agent-a",
            "action":"must use replacement","depends_on":[reference]}),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{}", text_of(&out));
        assert!(text_of(&out).contains("retired"), "{}", text_of(&out));
        assert_eq!(s.mem.graph().read().snapshot(), before);
    }
    s.mem.close().await.unwrap();
}

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
async fn a_cited_derive_and_an_id_citing_action_land_on_their_targets() {
    let s = server("mcp-cite").await;
    let ruling = ruling(&s).await;
    let out = call(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{
            "content": "flush retries keep the lease",
            "concept_type": "logic",
            "depends_on": [format::short_id(ruling)],
        }]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    let decision = s
        .mem
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == "flush retries keep the lease")
        .unwrap()
        .id;
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "retried a flush",
               "depends_on": [format::short_id(decision), ruling.0.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    {
        let g = s.mem.graph().read();
        let id = |content: &str| g.concepts().find(|c| c.content == content).unwrap().id;
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
        assert!(g
            .concepts()
            .all(|c| c.content != format::short_id(decision)));
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

    // Review M1: record_action keeps 0.5.0's text rule for an id that
    // names nothing (no layer holds it here): the entry is created as text.
    let unknown = NodeId::new().0.to_string();
    let out = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "ran it", "depends_on": [unknown.clone()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{}", text_of(&out));
    assert!(s
        .mem
        .graph()
        .read()
        .concepts()
        .any(|c| c.content == unknown));
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
    // #146 restores names with durable identity semantics.
    let named = serde_json::from_value::<DeriveParams>(json!({"agent_id": "agent-a",
        "concepts": [{"content": "a", "concept_type": "logic", "name": "a name"}]}))
    .unwrap();
    assert_eq!(named.concepts[0].name.as_deref(), Some("a name"));
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
