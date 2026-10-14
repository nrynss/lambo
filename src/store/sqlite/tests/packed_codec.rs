//! #55: `concepts.embedding` is packed little-endian `f32`, recorded in
//! `store_meta`; a store written by the text codec is refused until
//! `init_schema` (`lambo provision`) transcodes it, and a rerun is a no-op.

use super::vectors::seed_vectors;
use super::*;
use crate::store::vector::{encode_vector, SQLITE_VECTOR_CODEC};

const DIM: usize = 16;

/// Deterministic unit-ish vectors whose decimal forms are long (no short
/// round numbers), so a lossy transcode would show.
fn vector(seed: u32) -> Vec<f32> {
    (0..DIM)
        .map(|i| ((i as f32 + 1.0) * (seed as f32 * 0.37 + 0.11)).sin() / 3.0)
        .collect()
}

async fn blobs(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    sqlx::query_as("SELECT id, embedding FROM concepts WHERE embedding IS NOT NULL ORDER BY id")
        .fetch_all(store.pool())
        .await
        .unwrap()
}

async fn codec(store: &SqliteStore) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM store_meta WHERE key = 'vector_codec'")
        .fetch_optional(store.pool())
        .await
        .unwrap()
}

/// A store exactly as a pre-#55 build leaves it: every vector the CON-8 text
/// literal and no `store_meta` table. Returns the session and the vectors it
/// was written with.
async fn text_codec_store() -> (SqliteStore, SessionId, Vec<(NodeId, Vec<f32>)>) {
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("codec-legacy");
    let written: Vec<(NodeId, Vec<f32>)> = (1..=5).map(|n| (NodeId::new(), vector(n))).collect();
    let seeded: Vec<(NodeId, &str, Vec<f32>)> = written
        .iter()
        .enumerate()
        .map(|(n, (id, v))| (*id, ["a", "b", "c", "d", "e"][n], v.clone()))
        .collect();
    seed_vectors(&store, &sid, &vec_contract(DIM), &seeded).await;
    for (id, v) in &written {
        sqlx::query("UPDATE concepts SET embedding = ? WHERE id = ?")
            .bind(encode_vector(v).unwrap().into_bytes())
            .bind(id.0.to_string())
            .execute(store.pool())
            .await
            .unwrap();
    }
    sqlx::query("DROP TABLE store_meta")
        .execute(store.pool())
        .await
        .unwrap();
    (store, sid, written)
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// Flush and load round-trip `f32` bit-exactly, and the row is 4 bytes an
/// element (CON-8 on the packed form).
#[tokio::test]
async fn a_fresh_store_is_packed_and_round_trips_bit_exactly() {
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    assert_eq!(codec(&store).await.as_deref(), Some(SQLITE_VECTOR_CODEC));
    store.preflight_schema().await.unwrap();
    let sid = SessionId::from("codec-fresh");
    let id = NodeId::new();
    let mut v = vector(7);
    v[0] = f32::from_bits(1); // a subnormal, which decimal text is most likely to bend
    v[1] = -0.0;
    seed_vectors(&store, &sid, &vec_contract(DIM), &[(id, "x", v.clone())]).await;
    let rows = blobs(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.len(), DIM * 4);
    let loaded = store.load_session(&sid).await.unwrap();
    let back = loaded.concepts[0].embedding.clone().unwrap();
    assert_eq!(bits(&back), bits(&v));
}

/// The acceptance path: refused before migration (both shapes of an old
/// store), transcoded exactly by provision, readable after, and a rerun
/// writes nothing.
#[tokio::test]
async fn a_text_codec_store_is_refused_then_transcoded_once() {
    let (store, sid, written) = text_codec_store().await;

    // A pre-#55 store has no store_meta table: refused as unprovisioned.
    let err = store.preflight_schema().await.unwrap_err();
    assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("store_meta") && msg.contains("lambo provision"),
        "{msg}"
    );

    // The table without the marker (an interrupted provision): still refused,
    // by codec, not misread.
    sqlx::query(INIT_SQL).execute(store.pool()).await.unwrap();
    let err = store.preflight_schema().await.unwrap_err();
    assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("decimal text") && msg.contains("lambo provision"),
        "{msg}"
    );

    store.init_schema().await.unwrap();
    assert_eq!(codec(&store).await.as_deref(), Some(SQLITE_VECTOR_CODEC));
    store.preflight_schema().await.unwrap();
    let after = blobs(&store).await;
    assert_eq!(after.len(), written.len());
    assert!(after.iter().all(|(_, b)| b.len() == DIM * 4));
    let loaded = store.load_session(&sid).await.unwrap();
    for (id, v) in &written {
        let c = loaded.concepts.iter().find(|c| c.id == *id).unwrap();
        assert_eq!(bits(c.embedding.as_ref().unwrap()), bits(v), "{id:?}");
    }
    // The scan reads the transcoded rows too.
    let hits = store
        .vector_candidates_checked(&sid, &written[2].1, &vec_contract(DIM), 1)
        .await
        .unwrap();
    assert_eq!(hits[0].item, written[2].0);

    // Rerun: the marker says packed, so nothing is decoded or rewritten.
    store.init_schema().await.unwrap();
    assert_eq!(blobs(&store).await, after);
}

/// A live holder (a running pre-#55 writer) blocks the transcode, and the
/// refusal leaves the old codec and rows as they were.
#[tokio::test]
async fn a_live_lease_refuses_the_transcode_and_changes_nothing() {
    let (store, sid, _) = text_codec_store().await;
    let before = blobs(&store).await;
    sqlx::query(
        "INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
         VALUES (?, 'old-writer', strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%fZ','now','+60 seconds'))",
    )
    .bind(&sid.0)
    .execute(store.pool())
    .await
    .unwrap();
    let err = store.init_schema().await.unwrap_err();
    assert!(matches!(err, StoreError::Capability(_)), "{err:?}");
    assert!(err.to_string().contains("old-writer"), "{err}");
    assert_eq!(blobs(&store).await, before);
    assert_eq!(codec(&store).await, None);

    // Once the lease has expired, provision proceeds.
    sqlx::query("UPDATE session_leases SET expires_at = acquired_at")
        .execute(store.pool())
        .await
        .unwrap();
    store.init_schema().await.unwrap();
    assert_eq!(codec(&store).await.as_deref(), Some(SQLITE_VECTOR_CODEC));
}

/// A pre-#55 store with no vectors yet is still refused while held: the old
/// holder's first vector would be text in a store marked packed.
#[tokio::test]
async fn a_live_lease_refuses_the_marker_even_with_no_vectors() {
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    sqlx::query("DROP TABLE store_meta")
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO session_leases (session_id, holder, acquired_at, expires_at) \
         VALUES ('s', 'old-writer', strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                 strftime('%Y-%m-%dT%H:%M:%fZ','now','+60 seconds'))",
    )
    .execute(store.pool())
    .await
    .unwrap();
    assert!(store.init_schema().await.is_err());
    assert_eq!(codec(&store).await, None);
}

/// An erased session's tombstone (and a released row) never expires but
/// names no running holder, so it does not block the transcode.
#[tokio::test]
async fn reserved_lease_rows_do_not_block_the_transcode() {
    let (store, _, _) = text_codec_store().await;
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
    store.init_schema().await.unwrap();
    assert_eq!(codec(&store).await.as_deref(), Some(SQLITE_VECTOR_CODEC));
}

/// A codec this build does not know (a newer lambo's) is refused by both
/// provision and preflight, never overwritten.
#[tokio::test]
async fn an_unknown_codec_is_refused_not_overwritten() {
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    sqlx::query("UPDATE store_meta SET value = 'f16le' WHERE key = 'vector_codec'")
        .execute(store.pool())
        .await
        .unwrap();
    assert!(store.init_schema().await.is_err());
    assert!(store.preflight_schema().await.is_err());
    assert_eq!(codec(&store).await.as_deref(), Some("f16le"));
}

/// A corrupt text row fails the transcode whole: no row is half-migrated and
/// the marker is not written.
#[tokio::test]
async fn a_corrupt_text_row_rolls_the_transcode_back() {
    let (store, _, written) = text_codec_store().await;
    sqlx::query("UPDATE concepts SET embedding = ? WHERE id = ?")
        .bind(b"[1,0,oops]".to_vec())
        .bind(written[4].0 .0.to_string())
        .execute(store.pool())
        .await
        .unwrap();
    let before = blobs(&store).await;
    assert!(store.init_schema().await.is_err());
    assert_eq!(blobs(&store).await, before);
    assert_eq!(codec(&store).await, None);
}

/// #55 measurement, not a gate: a file store of 3,600 concepts at dim 1024
/// (the #26 store's shape), packed against text. Prints file sizes, session
/// load time on the packed store, the decode cost of the same rows in each
/// form, `load_session` (store read and decode, no graph build) on the packed
/// store, and the provision transcode time.
/// `cargo test --release --features store-sqlite,fixtures --lib \
///  store::sqlite::tests::packed_codec::codec_size_and_load_timing -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measurement; run in release with --nocapture"]
async fn codec_size_and_load_timing() {
    use std::time::Instant;
    const N: usize = 3_600;
    const W: usize = 1_024;
    let dir = std::env::temp_dir().join(format!("lambo-55-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.db");
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}", path.display());
    let store = SqliteStore::connect(&url)
        .unwrap()
        .with_vector_dim(W)
        .unwrap();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("codec-timing");
    let vectors: Vec<(NodeId, String, Vec<f32>)> = (0..N)
        .map(|n| {
            let v: Vec<f32> = (0..W)
                .map(|i| ((i as f32 + 1.0) * (n as f32 * 0.013 + 0.07)).sin() / 32.0)
                .collect();
            (NodeId::new(), format!("concept {n}"), v)
        })
        .collect();
    let seeded: Vec<(NodeId, &str, Vec<f32>)> = vectors
        .iter()
        .map(|(id, c, v)| (*id, c.as_str(), v.clone()))
        .collect();
    seed_vectors(&store, &sid, &vec_contract(W), &seeded).await;
    compact(&store).await;
    let packed_bytes = std::fs::metadata(&path).unwrap().len();
    let t = Instant::now();
    let loaded = store.load_session(&sid).await.unwrap();
    let load_packed = t.elapsed();
    assert_eq!(loaded.concepts.len(), N);
    let rows = blobs(&store).await;
    let t = Instant::now();
    for (id, b) in &rows {
        std::hint::black_box(crate::store::vector::decode_vector_blob(id, b).unwrap());
    }
    let decode_packed = t.elapsed();

    for (id, _, v) in &vectors {
        sqlx::query("UPDATE concepts SET embedding = ? WHERE id = ?")
            .bind(encode_vector(v).unwrap().into_bytes())
            .bind(id.0.to_string())
            .execute(store.pool())
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM store_meta")
        .execute(store.pool())
        .await
        .unwrap();
    compact(&store).await;
    let text_bytes = std::fs::metadata(&path).unwrap().len();
    let rows = blobs(&store).await;
    let t = Instant::now();
    for (id, b) in &rows {
        std::hint::black_box(crate::store::vector::decode_legacy_text_blob(id, b).unwrap());
    }
    let decode_text = t.elapsed();
    let t = Instant::now();
    store.init_schema().await.unwrap();
    let transcode = t.elapsed();

    println!(
        "codec_size_and_load_timing: {N} x {W}: file {:.1} MB text, {:.1} MB packed; \
         decode {decode_text:?} text, {decode_packed:?} packed; \
         load_session (packed) {load_packed:?}; provision transcode {transcode:?}",
        text_bytes as f64 / 1e6,
        packed_bytes as f64 / 1e6,
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

async fn compact(store: &SqliteStore) {
    sqlx::query("VACUUM").execute(store.pool()).await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(store.pool())
        .await
        .unwrap();
}
