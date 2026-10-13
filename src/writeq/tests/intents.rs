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
    })
    .bytes();
    assert!(cited > bare);
}

/// The v0.5.0 intent payload, frozen here byte for byte in shape (from tag
/// `v0.5.0`, `src/types/mod.rs`): what a downgraded writer decodes every
/// stored intent row with at load. `supplied` is kept opaque; only the
/// variant set and the field names matter to the decode.
mod v0_5_0 {
    use crate::types::ConceptType;

    #[allow(dead_code)]
    #[derive(Debug, PartialEq, serde::Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub(super) enum WriteIntentPayload {
        Derive {
            concepts: Vec<(String, ConceptType)>,
            pairs: Vec<(String, String)>,
        },
        DeriveImage {
            concepts: Vec<(String, ConceptType)>,
            pairs: Vec<(String, String)>,
            supplied: serde_json::Value,
        },
        Action {
            action: String,
            produces: Vec<String>,
            modifies: Vec<String>,
            depends_on: Vec<String>,
        },
    }
}

/// #36 PR 4 review H2: a settled cited derive is stored as the empty derive,
/// which the 0.5.0 decoder reads, so a downgrade still loads a session
/// whose cited writes all applied; an unconsumed one is refused by name
/// (Q18), never applied without its citations; an action's pins are
/// ignored by 0.5.0, which re-resolves its own way.
#[test]
fn a_settled_cited_intent_decodes_under_the_0_5_0_payload() {
    let cited = derive(PinnedCitations {
        references: vec![PinnedReference {
            content: "a decision".into(),
            reference: "8baad4e0".into(),
            target: NodeId::new(),
        }],
    })
    .to_intent_payload();
    let settled = serde_json::to_string(&cited.settled().expect("a cited derive settles")).unwrap();
    assert_eq!(settled, crate::types::SETTLED_IMAGE_INTENT_PAYLOAD);
    let old: v0_5_0::WriteIntentPayload = serde_json::from_str(&settled).unwrap();
    assert_eq!(
        old,
        v0_5_0::WriteIntentPayload::Derive {
            concepts: vec![],
            pairs: vec![],
        }
    );
    let unconsumed = serde_json::to_string(&cited).unwrap();
    let err = serde_json::from_str::<v0_5_0::WriteIntentPayload>(&unconsumed).unwrap_err();
    assert!(err.to_string().contains("derive_cited"), "{err}");
    // The settle pattern the SQL adapters match is this payload's prefix.
    assert!(unconsumed.starts_with(crate::types::DERIVE_CITED_PAYLOAD_LIKE.trim_end_matches('%')));

    let pinned_action =
        serde_json::to_string(&action(vec![Some(NodeId::new())]).to_intent_payload()).unwrap();
    assert!(matches!(
        serde_json::from_str::<v0_5_0::WriteIntentPayload>(&pinned_action).unwrap(),
        v0_5_0::WriteIntentPayload::Action { .. }
    ));
}

/// L1: a pinned payload with a field this build does not know is refused,
/// not decoded with the field dropped.
#[test]
fn an_unknown_pin_field_is_refused() {
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"references":[],"against":[]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"references":[{"content":"a","reference":"b","target":"00000000-0000-0000-0000-000000000001","edge":"x"}]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
}
