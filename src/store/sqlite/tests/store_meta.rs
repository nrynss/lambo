//! #176: `store_meta` rows written through the trait survive a reopen, sit
//! beside the codec row, and the canary pinned on one handle verifies on the
//! next.

use super::connection::TempDb;
use super::*;

#[tokio::test]
async fn store_meta_rows_round_trip_and_survive_a_reopen() {
    let db = TempDb::new();
    let path = db.path().to_str().unwrap().to_owned();
    {
        let store = SqliteStore::connect(&path).unwrap();
        store.init_schema().await.unwrap();
        assert_eq!(
            store
                .read_store_meta("embedder_fingerprint:x")
                .await
                .unwrap(),
            None
        );
        store
            .write_store_meta("embedder_fingerprint:x", "one")
            .await
            .unwrap();
        store
            .write_store_meta("embedder_fingerprint:x", "two")
            .await
            .unwrap();
        assert_eq!(
            store
                .read_store_meta("embedder_fingerprint:x")
                .await
                .unwrap()
                .as_deref(),
            Some("two")
        );
    }
    let store = SqliteStore::connect(&path).unwrap();
    store.preflight_schema().await.unwrap();
    assert_eq!(
        store
            .read_store_meta("embedder_fingerprint:x")
            .await
            .unwrap()
            .as_deref(),
        Some("two")
    );
    // The codec row is untouched beside it.
    assert_eq!(
        store
            .read_store_meta("vector_codec")
            .await
            .unwrap()
            .as_deref(),
        Some("f32le")
    );
}

#[cfg(feature = "embed-fixture")]
#[tokio::test]
async fn the_canary_pinned_on_one_handle_verifies_on_the_next() {
    use crate::embed::canary::{check, CanaryMode, CanaryOptions, CanaryVerdict};
    use crate::embed::{Embedder, FixtureEmbedder};
    let db = TempDb::new();
    let path = db.path().to_str().unwrap().to_owned();
    let embedder = FixtureEmbedder::new();
    let contract = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: embedder.dimensions(),
    };
    let options = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: None,
    };
    {
        let store = SqliteStore::connect(&path).unwrap();
        store.init_schema().await.unwrap();
        let out = check(&store, &embedder, &contract, &options, 0.85)
            .await
            .unwrap();
        assert_eq!(out.verdict, CanaryVerdict::Pinned);
    }
    let store = SqliteStore::connect(&path).unwrap();
    let out = check(&store, &embedder, &contract, &options, 0.85)
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Verified { min_cosine, .. } if min_cosine > 0.999_999),
        "{out:?}"
    );
}
