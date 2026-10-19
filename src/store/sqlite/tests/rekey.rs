//! #148 on SQLite: provision re-keys concepts the pre-#148 rule keyed, keeps
//! merged concepts merged, refuses while held, and a rerun is a no-op.

use super::*;
use crate::graph::canonical::{canonical_key, legacy_canonical_key};

async fn keys(store: &SqliteStore) -> Vec<(String, String)> {
    sqlx::query_as("SELECT content, canonical_key FROM concepts ORDER BY content")
        .fetch_all(store.pool())
        .await
        .unwrap()
}

/// A store as a pre-#148 build leaves it: every key the old rule's.
async fn legacy_store(contents: &[&str]) -> (SqliteStore, SessionId) {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("rekey");
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = NodeId::new();
    let mut mutations = vec![plant_interaction(&sid, origin, None, ts)];
    for c in contents {
        mutations.push(plant_concept(
            &sid,
            NodeId::new(),
            origin,
            c,
            ConceptType::Entity,
            ts,
        ));
    }
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations,
            },
            None,
        )
        .await
        .unwrap();
    for c in contents {
        sqlx::query("UPDATE concepts SET canonical_key = ? WHERE content = ?")
            .bind(legacy_canonical_key(c, |_| None))
            .bind(*c)
            .execute(store.pool())
            .await
            .unwrap();
    }
    (store, sid)
}

#[tokio::test]
async fn provision_rekeys_identifier_concepts_once() {
    let (store, sid) = legacy_store(&["2122de39e47", "release 2122de39e47e", "update users"]).await;
    let before = keys(&store).await;
    assert_eq!(before[1].1, "2122de39e47 releas", "the old rule stemmed it");

    let plan = store.rekey_canonical_keys(false).await.unwrap();
    assert_eq!(plan.changes.len(), 1);
    assert!(plan.collisions.is_empty());
    let after = keys(&store).await;
    for (content, key) in &after {
        assert_eq!(key, &canonical_key(content, |_| None), "{content}");
    }
    // Rerun: the store_meta marker skips the scan (#171), nothing written.
    assert_eq!(
        store.rekey_canonical_keys(false).await.unwrap(),
        crate::store::rekey::RekeyPlan {
            skipped: true,
            ..Default::default()
        }
    );
    assert_eq!(keys(&store).await, after);
    // A forced rerun checks every concept again and finds nothing to do.
    assert_eq!(
        store.rekey_canonical_keys(true).await.unwrap(),
        Default::default()
    );

    // The 12-character id now has its own key, distinct from the 11-character
    // concept's, so a write of it no longer lands on that concept.
    let snap = store.load_session(&sid).await.unwrap();
    let held: Vec<&str> = snap
        .concepts
        .iter()
        .map(|c| c.canonical_key.as_str())
        .collect();
    assert!(held.contains(&"2122de39e47"));
    assert!(!held.contains(&canonical_key("2122de39e47e", |_| None).as_str()));
}

#[tokio::test]
async fn a_live_lease_refuses_the_rekey_and_changes_nothing() {
    let (store, _) = legacy_store(&["release 2122de39e47e"]).await;
    let before = keys(&store).await;
    sqlx::query(
        "INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
         VALUES ('rekey', 'old-writer', strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%fZ','now','+60 seconds'))",
    )
    .execute(store.pool())
    .await
    .unwrap();
    let err = store.rekey_canonical_keys(false).await.unwrap_err();
    assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
    assert!(err.to_string().contains("lambo provision"), "{err}");
    assert_eq!(keys(&store).await, before);
}

/// Two concepts trading keys would trip the unique index mid-update without
/// the placeholder pass; and a new key already taken is reported, not merged.
#[tokio::test]
async fn a_taken_key_is_reported_and_left_alone() {
    let (store, _) = legacy_store(&["build 2122de39e47e"]).await;
    // A current build wrote the same text as a second concept before provision.
    let ts = Utc.timestamp_opt(1_752_000_100, 0).unwrap();
    let sid = SessionId::from("rekey");
    let origin = NodeId::new();
    let Mutation::UpsertNode {
        node: NodeKind::Concept(mut c),
    } = plant_concept(
        &sid,
        NodeId::new(),
        origin,
        "Build 2122de39e47e",
        ConceptType::Entity,
        ts,
    )
    else {
        unreachable!()
    };
    c.canonical_key = canonical_key("build 2122de39e47e", |_| None);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, origin, None, ts),
                    Mutation::UpsertNode {
                        node: NodeKind::Concept(c),
                    },
                ],
            },
            None,
        )
        .await
        .unwrap();
    // #171: a marker an earlier clean pass left behind.
    sqlx::query("INSERT INTO store_meta (key, value) VALUES (?, ?)")
        .bind(crate::store::rekey::REKEY_MARKER_KEY)
        .bind(crate::store::rekey::REKEY_MARKER_VALUE)
        .execute(store.pool())
        .await
        .unwrap();
    let before = keys(&store).await;
    let plan = store.rekey_canonical_keys(true).await.unwrap();
    assert!(plan.changes.is_empty());
    assert_eq!(plan.collisions.len(), 1);
    assert_eq!(keys(&store).await, before);
    // A collision withdraws the marker, so every provision reports it again.
    assert_eq!(marker(&store).await, None);
    let again = store.rekey_canonical_keys(false).await.unwrap();
    assert!(!again.skipped);
    assert_eq!(again.collisions.len(), 1);
}

/// The adapter-agnostic testkit the live Postgres test also runs.
#[tokio::test]
async fn the_shared_rekey_testkit_passes_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("rekey-kit");
    crate::store::rekey::testkit::plant_legacy(&store, &sid, None).await;
    crate::store::rekey::testkit::check_rekeyed(&store, &sid).await;
}

/// An erased session's tombstone (and a released row) never expires, but
/// names no running holder, so it does not block the re-key.
#[tokio::test]
async fn reserved_lease_rows_do_not_block_the_rekey() {
    let (store, _) = legacy_store(&["release 2122de39e47e"]).await;
    for (sid, holder) in [("gone", "lambo:erased"), ("done", "lambo:released")] {
        sqlx::query(
            "INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
             VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now'), '9999-12-31T00:00:00.000Z')",
        )
        .bind(sid)
        .bind(holder)
        .execute(store.pool())
        .await
        .unwrap();
    }
    assert_eq!(
        store
            .rekey_canonical_keys(false)
            .await
            .unwrap()
            .changes
            .len(),
        1
    );
}

/// Keys are caller-supplied, so a concept may already hold another row's
/// staging placeholder. Without the planner's check the first update pass
/// trips the unique index and provision rolls back; with it, that row keeps
/// its old key, is reported, and every other row is still re-keyed.
#[tokio::test]
async fn an_occupied_placeholder_is_reported_and_the_rest_rekeyed() {
    let (store, _) =
        legacy_store(&["build 2122de39e47e", "release 2122de39e47e", "squatter"]).await;
    let id: String = sqlx::query_scalar("SELECT id FROM concepts WHERE content = ?")
        .bind("build 2122de39e47e")
        .fetch_one(store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE concepts SET canonical_key = ? WHERE content = 'squatter'")
        .bind(crate::store::rekey::placeholder_key(&id))
        .execute(store.pool())
        .await
        .unwrap();
    let plan = store.rekey_canonical_keys(false).await.unwrap();
    assert_eq!(plan.collisions, vec![id], "{plan:?}");
    assert_eq!(plan.changes.len(), 1, "{plan:?}");
    let after = keys(&store).await;
    assert_eq!(
        after[0],
        (
            "build 2122de39e47e".to_string(),
            legacy_canonical_key("build 2122de39e47e", |_| None)
        )
    );
    assert_eq!(after[1].1, canonical_key("release 2122de39e47e", |_| None));
}

async fn marker(store: &SqliteStore) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM store_meta WHERE key = ?")
        .bind(crate::store::rekey::REKEY_MARKER_KEY)
        .fetch_optional(store.pool())
        .await
        .unwrap()
}

/// #171: a clean re-key records the marker, and while it stands provision
/// reads no concept: a legacy key planted after it (what a pre-#148 writer
/// would leave) is not seen until a forced re-key, which re-keys it.
#[tokio::test]
async fn a_clean_rekey_is_recorded_and_skipped_until_forced() {
    let (store, sid) = legacy_store(&["release 2122de39e47e"]).await;
    assert_eq!(marker(&store).await, None, "init_schema records no re-key");
    assert_eq!(
        store
            .rekey_canonical_keys(false)
            .await
            .unwrap()
            .changes
            .len(),
        1
    );
    assert_eq!(
        marker(&store).await.as_deref(),
        Some(crate::store::rekey::REKEY_MARKER_VALUE)
    );

    let content = "deploy 2122de39e47e";
    let ts = Utc.timestamp_opt(1_752_000_200, 0).unwrap();
    let origin = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, origin, None, ts),
                    plant_concept(
                        &sid,
                        NodeId::new(),
                        origin,
                        content,
                        ConceptType::Entity,
                        ts,
                    ),
                ],
            },
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE concepts SET canonical_key = ? WHERE content = ?")
        .bind(legacy_canonical_key(content, |_| None))
        .bind(content)
        .execute(store.pool())
        .await
        .unwrap();

    let skipped = store.rekey_canonical_keys(false).await.unwrap();
    assert!(skipped.skipped && skipped.changes.is_empty(), "{skipped:?}");
    let forced = store.rekey_canonical_keys(true).await.unwrap();
    assert!(!forced.skipped);
    assert_eq!(forced.changes.len(), 1, "{forced:?}");
    let after = keys(&store).await;
    assert!(
        after.iter().all(|(c, k)| k == &canonical_key(c, |_| None)),
        "{after:?}"
    );
}

/// #171: an empty store's first re-key is clean too, so it records the
/// marker without a scan of anything; a refused re-key records nothing.
#[tokio::test]
async fn a_refused_rekey_records_no_marker_and_an_empty_one_does() {
    let (store, _) = legacy_store(&["release 2122de39e47e"]).await;
    sqlx::query(
        "INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
         VALUES ('rekey', 'old-writer', strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%fZ','now','+60 seconds'))",
    )
    .execute(store.pool())
    .await
    .unwrap();
    store.rekey_canonical_keys(false).await.unwrap_err();
    assert_eq!(marker(&store).await, None);

    let empty = test_store();
    empty.init_schema().await.unwrap();
    assert_eq!(
        empty.rekey_canonical_keys(false).await.unwrap(),
        Default::default()
    );
    assert!(empty.rekey_canonical_keys(false).await.unwrap().skipped);
}

/// #171: a marker recorded under another key rule is not this rule's, so
/// provision re-keys and records the current value.
#[tokio::test]
async fn a_marker_from_another_key_rule_re_keys() {
    let (store, _) = legacy_store(&["release 2122de39e47e"]).await;
    sqlx::query("INSERT INTO store_meta (key, value) VALUES (?, ?)")
        .bind(crate::store::rekey::REKEY_MARKER_KEY)
        .bind("147-some-earlier-rule")
        .execute(store.pool())
        .await
        .unwrap();
    let plan = store.rekey_canonical_keys(false).await.unwrap();
    assert!(!plan.skipped, "{plan:?}");
    assert_eq!(plan.changes.len(), 1, "{plan:?}");
    assert_eq!(
        marker(&store).await.as_deref(),
        Some(crate::store::rekey::REKEY_MARKER_VALUE)
    );
}
