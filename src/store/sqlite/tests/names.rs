use super::*;

/// Provisioning an old populated store adds only the new metadata tables; preview
/// preflight itself remains read-only and refuses the unprovisioned shape.
#[tokio::test]
async fn names_provision_upgrades_a_populated_legacy_store() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("legacy-names-provision");
    let at = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    let i = NodeId::new();
    let c = NodeId::new();
    let mut batch = MutationBatch::new();
    batch.push(plant_interaction(&sid, i, None, at));
    batch.push(plant_concept(
        &sid,
        c,
        i,
        "standing rule",
        ConceptType::Logic,
        at,
    ));
    store.flush(&batch, None).await.unwrap();
    let before = store.load_session(&sid).await.unwrap();
    sqlx::query("DROP TABLE concept_names")
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("DROP TABLE retired_aliases")
        .execute(store.pool())
        .await
        .unwrap();
    let err = store.preflight_schema().await.unwrap_err().to_string();
    assert!(
        err.contains("concept_names")
            && err.contains("retired_aliases")
            && err.contains("provision"),
        "{err}"
    );
    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
    store.preflight_schema().await.unwrap();
    assert_eq!(store.load_session(&sid).await.unwrap(), before);
}

#[cfg(feature = "fixtures")]
#[tokio::test]
async fn names_snapshot_seed_preserves_identity_and_rejects_retargeting_atomically() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let mut snapshot = GraphSnapshot {
        session_id: SessionId::from("names-seed"),
        ..Default::default()
    };
    let name = crate::types::ConceptName {
        session_id: snapshot.session_id.clone(),
        name: "standing rule".into(),
        name_key: crate::graph::canonical::canonical_key("standing rule", |_| None),
        node_id: NodeId::new(),
    };
    snapshot.names.push(name.clone());
    store.seed(&snapshot).await.unwrap();
    let before = store.load_session(&snapshot.session_id).await.unwrap();
    snapshot.names[0].node_id = NodeId::new();
    assert!(store.seed(&snapshot).await.is_err());
    assert_eq!(
        store.load_session(&snapshot.session_id).await.unwrap(),
        before
    );
    assert_eq!(before.names, vec![name]);
}
