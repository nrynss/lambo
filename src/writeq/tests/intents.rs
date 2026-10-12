//! #36 PR 4: what a job's durable intent carries, and that a request without
//! citations or id-shaped `depends_on` entries carries exactly what it did
//! before.

use super::admission::JobPayload;
use crate::types::{ConceptType, NodeId, PinnedCitations, PinnedReference, WriteIntentPayload};

fn derive(citations: PinnedCitations) -> JobPayload {
    JobPayload::Derive {
        concepts: vec![("a decision".into(), ConceptType::Logic)],
        pairs: vec![],
        citations,
    }
}

fn action(pins: Vec<Option<NodeId>>) -> JobPayload {
    JobPayload::Action {
        action: "ran it".into(),
        produces: vec![],
        modifies: vec![],
        depends_on: vec!["schema v1".into()],
        depends_on_pins: pins,
    }
}

/// Legacy requests serialize byte for byte as before 0.6.0, so a store's
/// intent rows and an older build's decoding are unchanged.
#[test]
fn legacy_payloads_are_byte_identical() {
    let json =
        serde_json::to_string(&derive(PinnedCitations::default()).to_intent_payload()).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"derive","concepts":[["a decision","Logic"]],"pairs":[]}"#
    );
    let json = serde_json::to_string(&action(vec![]).to_intent_payload()).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"action","action":"ran it","produces":[],"modifies":[],"depends_on":["schema v1"]}"#
    );
    // And an intent written before 0.6.0 decodes with nothing pinned.
    let old: WriteIntentPayload = serde_json::from_str(&json).unwrap();
    match JobPayload::from_intent_payload(old) {
        JobPayload::Action {
            depends_on_pins, ..
        } => assert!(depends_on_pins.is_empty()),
        other => panic!("{other:?}"),
    }
}

/// A cited derive is its own variant (Q18: an older build refuses to decode
/// it rather than apply it without its citations), and its pins round-trip.
#[test]
fn a_cited_derive_is_its_own_variant_and_round_trips() {
    let target = NodeId(uuid::Uuid::from_u128(7));
    let citations = PinnedCitations {
        references: vec![PinnedReference {
            content: "a decision".into(),
            reference: "00000000".into(),
            target,
        }],
        names: vec![("a decision".into(), "a name".into())],
    };
    let intent = derive(citations.clone()).to_intent_payload();
    let json = serde_json::to_string(&intent).unwrap();
    assert!(json.starts_with(r#"{"kind":"derive_cited","#), "{json}");
    let back: WriteIntentPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(back, intent);
    match JobPayload::from_intent_payload(back) {
        JobPayload::Derive { citations: c, .. } => assert_eq!(c, citations),
        other => panic!("{other:?}"),
    }
    // The pins of an action that named a concept by id ride along too.
    let json = serde_json::to_string(&action(vec![Some(target)]).to_intent_payload()).unwrap();
    assert!(json.contains(r#""depends_on_pins":["#), "{json}");
}

/// The byte admission charges the citations a job carries.
#[test]
fn a_cited_job_is_charged_its_citations() {
    let bare = derive(PinnedCitations::default()).bytes();
    let cited = derive(PinnedCitations {
        references: vec![PinnedReference {
            content: "a decision".into(),
            reference: "a long reference to a ruling".into(),
            target: NodeId::new(),
        }],
        names: vec![],
    })
    .bytes();
    assert!(cited > bare);
}
