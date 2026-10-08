//! Row codecs: enum column strings and session embedding decode.

use super::*;

#[test]
fn enum_column_strings_roundtrip_all_variants() {
    for ct in [
        ConceptType::Entity,
        ConceptType::Logic,
        ConceptType::Constraint,
        ConceptType::Resource,
        ConceptType::Observation,
    ] {
        assert_eq!(parse_concept_type(concept_type_sql(ct)).unwrap(), ct);
    }
    for et in [
        EdgeType::Temporal,
        EdgeType::Derives,
        EdgeType::CoOccurrence,
        EdgeType::Causal,
        EdgeType::Dependency,
        EdgeType::Hierarchical,
        EdgeType::Semantic,
    ] {
        assert_eq!(parse_edge_type(edge_type_sql(et)).unwrap(), et);
    }
    for cs in [
        CanonizationStatus::None,
        CanonizationStatus::Candidate,
        CanonizationStatus::Venerable,
        CanonizationStatus::Canonical,
    ] {
        assert_eq!(
            parse_canonization_status(canonization_status_sql(cs)).unwrap(),
            cs
        );
    }
    assert!(parse_concept_type("Bogus").is_err());
    assert!(parse_edge_type("Bogus").is_err());
    assert!(parse_canonization_status("Bogus").is_err());
}

#[test]
fn session_embedding_xor_corruption_errors_not_silent_none() {
    // STORE-7: a sessions row with embedding_dim set but embedding_kind NULL
    // (what direct SQL on a corrupt/migrated row would produce) must error like
    // sqlite, not silently return `embedding: None`.
    let sid = "session-store7";
    // The old silent-None shape (kind absent, dim present) — the STORE-7 bug.
    let err = session_embedding_from_parts(None, None, Some(1024), sid).unwrap_err();
    // E2E-2: deterministic corruption classifies as `Invariant`, so
    // `tx_retry` returns on the first attempt instead of replaying it 5×.
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("embedding_dim without embedding_kind"),
        "corruption error must name the shape: {err}"
    );
    // Mirror image: kind present, dim absent — sqlite errors here too.
    let err = session_embedding_from_parts(Some("bge_m3".into()), None, None, sid).unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("embedding_kind without embedding_dim"),
        "{err}"
    );
    // Negative dim is an error, not an `as usize` wrap (sqlite parity).
    let err = session_embedding_from_parts(Some("bge_m3".into()), None, Some(-1), sid).unwrap_err();
    assert!(err.to_string().contains("negative embedding_dim"), "{err}");
    // Well-formed rows still parse.
    let got = session_embedding_from_parts(
        Some("bge_m3".into()),
        Some("BAAI/bge-m3".into()),
        Some(1024),
        sid,
    )
    .unwrap();
    let stored = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("BAAI/bge-m3".into()),
        dim: 1024,
    };
    assert_eq!(got, Some(stored.clone()));
    let live = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("renamed-bge-m3.gguf".into()),
        dim: 1024,
    };
    let err =
        crate::resolve::assert_session_embedding_compatible(Some(&stored), &live).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("BAAI/bge-m3"), "{text}");
    assert!(text.contains("renamed-bge-m3.gguf"), "{text}");
    assert!(text.contains("--allow-embedding-mismatch"), "{text}");
    assert_eq!(
        session_embedding_from_parts(None, None, None, sid).unwrap(),
        None
    );
}
