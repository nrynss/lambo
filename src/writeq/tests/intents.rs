//! #36 PR 4 and #19(b): what a job's durable intent carries, and that a
//! request without citations, contradictions or id-shaped `depends_on`
//! entries carries exactly what it did before.

use super::admission::JobPayload;
use crate::types::{ConceptType, NodeId, PinnedCitations, PinnedReference, WriteIntentPayload};

fn derive(citations: PinnedCitations) -> JobPayload {
    JobPayload::Derive {
        concepts: vec![("a decision".into(), ConceptType::Logic)],
        pairs: vec![],
        citations,
        repo_scope: None,
    }
}

fn action(pins: Vec<Option<NodeId>>) -> JobPayload {
    action_against(pins, Vec::new())
}

fn action_against(pins: Vec<Option<NodeId>>, against: Vec<PinnedReference>) -> JobPayload {
    JobPayload::Action {
        repo_scope: None,
        action: "ran it".into(),
        produces: vec![],
        modifies: vec![],
        depends_on: vec!["schema v1".into()],
        against,
        depends_on_pins: pins,
    }
}

fn pinned(content: &str, reference: &str, n: u128) -> PinnedReference {
    PinnedReference {
        content: content.into(),
        reference: reference.into(),
        target: NodeId(uuid::Uuid::from_u128(n)),
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
    match JobPayload::from_intent_payload(old).unwrap() {
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
        against: Vec::new(),
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
    match JobPayload::from_intent_payload(back).unwrap() {
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
        against: Vec::new(),
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
        against: Vec::new(),
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
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"references":[],"supersedes":[]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
    // #19(b): `against` is known now, and its entries refuse a field too.
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"references":[],"against":[]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_ok());
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"against":[{"content":"a","reference":"b","target":"00000000-0000-0000-0000-000000000001","edge":"x"}]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
    let json = r#"{"kind":"action_cited","action":"a","produces":[],"modifies":[],"depends_on":[],"against":[{"content":"a","reference":"b","target":"00000000-0000-0000-0000-000000000001","edge":"x"}]}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
    let json = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"references":[{"content":"a","reference":"b","target":"00000000-0000-0000-0000-000000000001","edge":"x"}]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(json).is_err());
}

fn scoped(mut job: JobPayload, scope: &str) -> JobPayload {
    match &mut job {
        JobPayload::Derive { repo_scope, .. } | JobPayload::Action { repo_scope, .. } => {
            *repo_scope = Some(scope.into());
        }
        JobPayload::DeriveImage { .. } => unreachable!("not used here"),
    }
    job
}

/// #129: a scoped job is one `repo_scoped` wrapper around the unscoped
/// payload, so an older build refuses it instead of replaying it under the
/// legacy path key; it round-trips, is charged its scope, and settles to the
/// payload every build decodes.
#[test]
fn repo_scoped_intents_round_trip_and_refuse_downgrade() {
    let cited = PinnedCitations {
        references: vec![pinned("a decision", "README.md", 9)],
        against: vec![],
    };
    // #19(b): contradictions ride inside the wrapped payload too.
    let contradicting = PinnedCitations {
        references: vec![],
        against: vec![pinned("a decision", "docs/old.md", 10)],
    };
    let jobs = [
        derive(PinnedCitations::default()),
        derive(cited),
        derive(contradicting),
        action(vec![]),
        action_against(vec![], vec![pinned("ran it", "src/lib.rs", 11)]),
    ];
    for unscoped in jobs {
        let bare_bytes = unscoped.bytes();
        let inner = unscoped.to_intent_payload();
        let job = scoped(unscoped, "host/owner/repo");
        let intent = job.to_intent_payload();
        assert_eq!(
            intent,
            WriteIntentPayload::RepoScoped {
                repo_scope: "host/owner/repo".into(),
                payload: Box::new(inner),
            }
        );
        let json = serde_json::to_string(&intent).unwrap();
        assert!(
            json.starts_with(crate::types::REPO_SCOPED_PAYLOAD_LIKE.trim_end_matches('%')),
            "{json}"
        );
        // The scope is stored once, on the wrapper.
        assert_eq!(json.matches("host/owner/repo").count(), 1, "{json}");
        assert!(serde_json::from_str::<v0_5_0::WriteIntentPayload>(&json).is_err());
        let replay = JobPayload::from_intent_payload(serde_json::from_str(&json).unwrap()).unwrap();
        assert_eq!(replay.to_intent_payload(), intent);
        let settled = serde_json::to_string(&intent.settled().unwrap()).unwrap();
        assert_eq!(settled, crate::types::SETTLED_IMAGE_INTENT_PAYLOAD);
        assert!(serde_json::from_str::<v0_5_0::WriteIntentPayload>(&settled).is_ok());
        assert_eq!(job.bytes(), bare_bytes + "host/owner/repo".len());
    }
}

#[test]
fn repo_scoped_replay_refuses_invalid_nested_or_image_payloads() {
    let bare = WriteIntentPayload::Derive {
        concepts: vec![],
        pairs: vec![],
    };
    let wrap = |scope: &str, payload: WriteIntentPayload| WriteIntentPayload::RepoScoped {
        repo_scope: scope.into(),
        payload: Box::new(payload),
    };
    for scope in ["", "  ", "a\0b", "a\u{202e}b"] {
        assert!(
            JobPayload::from_intent_payload(wrap(scope, bare.clone())).is_err(),
            "{scope:?}"
        );
    }
    let nested = wrap("outer", wrap("inner", bare.clone()));
    assert!(JobPayload::from_intent_payload(nested).is_err());
    // An image derive is never scoped: a stored one is refused, not applied
    // without its scope.
    use crate::types::{
        EmbeddingContract, EmbeddingSource, SourceModality, SuppliedVector, VectorOrigin,
    };
    let image = WriteIntentPayload::DeriveImage {
        concepts: vec![("red [image:a1]".into(), ConceptType::Resource)],
        pairs: vec![],
        supplied: SuppliedVector {
            content: "red [image:a1]".into(),
            vector: vec![0.6, 0.8],
            contract: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 2,
            },
            source: EmbeddingSource {
                modality: SourceModality::Image,
                origin: VectorOrigin::Client,
                sha256: None,
                mime: None,
            },
        },
    };
    assert!(JobPayload::from_intent_payload(wrap("repo", image)).is_err());
    // The scope lives only on the wrapper: citations carrying one are
    // refused at decode (`deny_unknown_fields`).
    let stray = r#"{"kind":"derive_cited","concepts":[],"pairs":[],"citations":{"repo_scope":"r","references":[]}}"#;
    assert!(serde_json::from_str::<WriteIntentPayload>(stray).is_err());
}

/// #19(b): a derive's contradictions ride in the cited intent beside its
/// citations, and a derive that only cites serializes exactly as #36 PR 4
/// wrote it (no `against` key), so the two can be told apart and neither
/// changes the other's bytes.
#[test]
fn a_derive_s_contradictions_ride_in_the_cited_intent() {
    let cites = derive(PinnedCitations {
        references: vec![pinned("a decision", "00000000", 7)],
        against: vec![],
    })
    .to_intent_payload();
    assert_eq!(
        serde_json::to_string(&cites).unwrap(),
        r#"{"kind":"derive_cited","concepts":[["a decision","Logic"]],"pairs":[],"citations":{"references":[{"content":"a decision","reference":"00000000","target":"00000000-0000-0000-0000-000000000007"}]}}"#
    );
    let both = PinnedCitations {
        references: vec![pinned("a decision", "00000000", 7)],
        against: vec![pinned("a decision", "an older belief", 8)],
    };
    let only_against = PinnedCitations {
        references: vec![],
        against: vec![pinned("a decision", "an older belief", 8)],
    };
    for citations in [both, only_against] {
        let intent = derive(citations.clone()).to_intent_payload();
        let json = serde_json::to_string(&intent).unwrap();
        assert!(json.starts_with(r#"{"kind":"derive_cited","#), "{json}");
        assert!(
            json.contains(r#""against":[{"content":"a decision","reference":"an older belief""#),
            "{json}"
        );
        assert_eq!(
            json.contains(r#""references""#),
            !citations.references.is_empty(),
            "{json}"
        );
        let back: WriteIntentPayload = serde_json::from_str(&json).unwrap();
        match JobPayload::from_intent_payload(back).unwrap() {
            JobPayload::Derive { citations: c, .. } => assert_eq!(c, citations),
            other => panic!("{other:?}"),
        }
    }
    // Charged for, like a citation.
    let bare = derive(PinnedCitations::default()).bytes();
    let contradicting = derive(PinnedCitations {
        references: vec![],
        against: vec![pinned("a decision", "an older belief", 8)],
    })
    .bytes();
    assert!(contradicting > bare);
}

/// #19(b): an action with `against` is its own kind (Q18), round-trips with
/// its pins, and an action without it is the plain kind, byte for byte.
#[test]
fn an_action_with_against_is_its_own_variant_and_round_trips() {
    let against = vec![pinned("ran it", "chargers are fine", 9)];
    let job = action_against(
        vec![Some(NodeId(uuid::Uuid::from_u128(3)))],
        against.clone(),
    );
    let intent = job.to_intent_payload();
    let json = serde_json::to_string(&intent).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"action_cited","action":"ran it","produces":[],"modifies":[],"depends_on":["schema v1"],"depends_on_pins":["00000000-0000-0000-0000-000000000003"],"against":[{"content":"ran it","reference":"chargers are fine","target":"00000000-0000-0000-0000-000000000009"}]}"#
    );
    let back: WriteIntentPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(back, intent);
    match JobPayload::from_intent_payload(back).unwrap() {
        JobPayload::Action {
            against: a,
            depends_on_pins,
            ..
        } => {
            assert_eq!(a, against);
            assert_eq!(depends_on_pins.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    assert!(job.bytes() > action(vec![Some(NodeId(uuid::Uuid::from_u128(3)))]).bytes());
    let plain = serde_json::to_string(&action(vec![]).to_intent_payload()).unwrap();
    assert!(plain.starts_with(r#"{"kind":"action","#), "{plain}");
    assert!(!plain.contains("against"), "{plain}");
}

/// #19(b), the #36 PR 4 H2 precedent: a settled derive or action carrying
/// contradictions is stored as the empty derive the frozen 0.5.0 decoder
/// reads, and an unconsumed one is refused by name, never applied without
/// its contradictions.
#[test]
fn a_settled_contradicting_intent_decodes_under_the_0_5_0_payload() {
    let derive_against = derive(PinnedCitations {
        references: vec![],
        against: vec![pinned("a decision", "an older belief", 8)],
    })
    .to_intent_payload();
    let action_cited =
        action_against(vec![], vec![pinned("ran it", "chargers are fine", 9)]).to_intent_payload();
    for (intent, kind, like) in [
        (
            &derive_against,
            "derive_cited",
            crate::types::DERIVE_CITED_PAYLOAD_LIKE,
        ),
        (
            &action_cited,
            "action_cited",
            crate::types::ACTION_CITED_PAYLOAD_LIKE,
        ),
    ] {
        let settled = serde_json::to_string(&intent.settled().expect("it settles")).unwrap();
        assert_eq!(settled, crate::types::SETTLED_IMAGE_INTENT_PAYLOAD);
        assert_eq!(
            serde_json::from_str::<v0_5_0::WriteIntentPayload>(&settled).unwrap(),
            v0_5_0::WriteIntentPayload::Derive {
                concepts: vec![],
                pairs: vec![],
            }
        );
        let unconsumed = serde_json::to_string(intent).unwrap();
        let err = serde_json::from_str::<v0_5_0::WriteIntentPayload>(&unconsumed).unwrap_err();
        assert!(err.to_string().contains(kind), "{err}");
        // The settle pattern the SQL adapters match is this payload's prefix,
        // and matches no other kind.
        assert!(
            unconsumed.starts_with(like.trim_end_matches('%')),
            "{unconsumed}"
        );
    }
    let plain = serde_json::to_string(&action(vec![]).to_intent_payload()).unwrap();
    assert!(!plain.starts_with(crate::types::ACTION_CITED_PAYLOAD_LIKE.trim_end_matches('%')));
}
