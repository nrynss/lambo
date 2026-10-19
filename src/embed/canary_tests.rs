//! #176: the canary fingerprint — pin, verify, drift, jitter, modes, the
//! #87 threshold and its cache. Deterministic: every vector comes from a
//! seeded hash of its text.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::embed::EmbedError;

const DIM: usize = 64;

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A pseudo-random direction for `text`, not normalized.
fn base(text: &str) -> Vec<f64> {
    let mut seed = 0xcbf2_9ce4_8422_2325u64;
    for b in text.bytes() {
        seed = (seed ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (0..DIM as u64)
        .map(|i| {
            let r = splitmix(seed ^ splitmix(i));
            (r >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        })
        .collect()
}

fn unit(v: Vec<f64>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    v.into_iter().map(|x| (x / n) as f32).collect()
}

/// Deterministic test embedder. `drift` adds that much of a second,
/// text-specific direction (a "model change"); `rel:<i>:<side>` texts share
/// the `rel:<i>` direction, so pairs of them are related but distinct.
struct Probe {
    drift: f64,
    fail: bool,
    calls: AtomicUsize,
}

impl Probe {
    fn new(drift: f64) -> Self {
        Self {
            drift,
            fail: false,
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Embedder for Probe {
    fn dimensions(&self) -> usize {
        DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(EmbedError::Unavailable("probe is down".into()));
        }
        let mut v = match text.strip_prefix("rel:") {
            Some(rest) => {
                let group = rest.split(':').next().unwrap_or("");
                let shared = base(&format!("rel:{group}"));
                let own = base(text);
                shared.iter().zip(&own).map(|(s, o)| s + 0.45 * o).collect()
            }
            None => base(text),
        };
        if self.drift != 0.0 {
            let d = base(&format!("drift:{text}"));
            let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            for (x, y) in v.iter_mut().zip(d) {
                *x += self.drift * n * y / (DIM as f64 / 3.0).sqrt();
            }
        }
        Ok(unit(v))
    }
}

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "probe".into(),
        model: Some("m1".into()),
        dim: DIM,
    }
}

fn warn() -> CanaryOptions {
    CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: None,
    }
}

fn store() -> crate::MemoryStore {
    crate::MemoryStore::new()
}

#[tokio::test]
async fn the_first_check_pins_and_the_second_verifies() {
    let store = store();
    let probe = Probe::new(0.0);
    let first = check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(first.verdict, CanaryVerdict::Pinned);
    assert_eq!(first.merge_threshold, 0.85);
    let raw = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .expect("pinned");
    let fp: Fingerprint = serde_json::from_str(&raw).unwrap();
    assert_eq!(fp.vectors.len(), CANARY_PHRASES.len());
    assert_eq!(fp.random_pair.pairs, 190);
    assert!(
        fp.random_pair.p99 < 0.6,
        "unrelated canaries: {:?}",
        fp.random_pair
    );

    let second = check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    match second.verdict {
        CanaryVerdict::Verified { min_cosine, .. } => assert!(min_cosine > 0.999_999),
        other => panic!("{other:?}"),
    }
    // Verifying writes nothing: the pin is the one the first check made.
    let again = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap();
    assert_eq!(again.as_deref(), Some(raw.as_str()));
}

#[tokio::test]
async fn a_model_change_behind_the_same_name_is_a_drift() {
    let store = store();
    check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    let pinned = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap();
    let out = check(&store, &Probe::new(0.2), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    match out.verdict {
        CanaryVerdict::Drifted { min_cosine, worst } => {
            assert!(min_cosine < CANARY_MIN_COSINE, "{min_cosine}");
            assert!(worst < CANARY_PHRASES.len());
        }
        other => panic!("{other:?}"),
    }
    // A drift never overwrites the pin it failed against.
    let after = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap();
    assert_eq!(after, pinned);
}

#[tokio::test]
async fn numerical_jitter_is_not_a_drift() {
    let store = store();
    check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    let out = check(&store, &Probe::new(0.01), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Verified { min_cosine, .. } if min_cosine >= CANARY_MIN_COSINE),
        "{out:?}"
    );
}

#[tokio::test]
async fn off_embeds_and_writes_nothing() {
    let store = store();
    let probe = Probe::new(0.0);
    let out = check(&store, &probe, &contract(), &CanaryOptions::off(), 0.85)
        .await
        .unwrap();
    assert_eq!(out.verdict, CanaryVerdict::Off);
    assert_eq!(probe.calls(), 0);
    assert!(store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn an_unreachable_embedder_is_unverified_and_pins_nothing() {
    let store = store();
    let mut probe = Probe::new(0.0);
    probe.fail = true;
    let out = check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Unverified(_)),
        "{out:?}"
    );
    assert_eq!(out.merge_threshold, 0.85);
    assert!(store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn each_contract_keeps_its_own_fingerprint() {
    let store = store();
    let other = EmbeddingContract {
        model: Some("m2".into()),
        ..contract()
    };
    check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    // A different (drifted-looking) embedder under another contract pins its
    // own row rather than failing against the first.
    let out = check(&store, &Probe::new(0.3), &other, &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(out.verdict, CanaryVerdict::Pinned);
    let back = check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert!(matches!(back.verdict, CanaryVerdict::Verified { .. }));
}

#[tokio::test]
async fn another_canary_version_or_a_corrupt_row_is_pinned_again() {
    let store = store();
    let key = fingerprint_key(&contract());
    check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    let mut fp: Fingerprint =
        serde_json::from_str(&store.read_store_meta(&key).await.unwrap().unwrap()).unwrap();
    fp.version = CANARY_VERSION + 1;
    store
        .write_store_meta(&key, &serde_json::to_string(&fp).unwrap())
        .await
        .unwrap();
    let out = check(&store, &Probe::new(0.3), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(out.verdict, CanaryVerdict::Pinned);

    store.write_store_meta(&key, "{not json").await.unwrap();
    let out = check(&store, &Probe::new(0.0), &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(out.verdict, CanaryVerdict::Pinned);
}

/// A per-test directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("lambo-canary-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_pairs(dir: &std::path::Path, n: usize, tag: &str) -> PathBuf {
    let pairs: Vec<_> = (0..n)
        .map(
            |i| serde_json::json!({"a": format!("rel:{tag}{i}:a"), "b": format!("rel:{tag}{i}:b")}),
        )
        .collect();
    let path = dir.join(format!("pairs-{tag}-{n}.json"));
    std::fs::write(&path, serde_json::to_vec(&pairs).unwrap()).unwrap();
    path
}

#[tokio::test]
async fn the_merge_threshold_is_the_p99_of_enough_related_pairs_and_is_cached() {
    let dir = Scratch::new();
    let store = store();
    let probe = Probe::new(0.0);
    let options = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(write_pairs(dir.path(), 120, "x")),
    };
    let out = check(&store, &probe, &contract(), &options, 0.85)
        .await
        .unwrap();
    assert_eq!(
        out.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 120 }
    );

    // The value is the nearest-rank p99 of the pair cosines.
    let mut cosines = Vec::new();
    for i in 0..120 {
        let a = probe.embed(&format!("rel:x{i}:a")).await.unwrap();
        let b = probe.embed(&format!("rel:x{i}:b")).await.unwrap();
        cosines.push(cosine64(&a, &b));
    }
    let expected = pair_quantiles(cosines).unwrap().p99;
    assert_eq!(out.merge_threshold, expected);
    let fp: Fingerprint = serde_json::from_str(
        &store
            .read_store_meta(&fingerprint_key(&contract()))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(out.merge_threshold > fp.random_pair.p99);

    // Cached: a second attach embeds only the canaries.
    let before = probe.calls();
    let again = check(&store, &probe, &contract(), &options, 0.85)
        .await
        .unwrap();
    assert_eq!(again.merge_threshold, expected);
    assert_eq!(probe.calls() - before, CANARY_PHRASES.len());

    // A changed file is measured again.
    let changed = CanaryOptions {
        related_pairs: Some(write_pairs(dir.path(), 130, "y")),
        ..options.clone()
    };
    let before = probe.calls();
    let third = check(&store, &probe, &contract(), &changed, 0.85)
        .await
        .unwrap();
    assert_eq!(
        third.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 130 }
    );
    assert_eq!(probe.calls() - before, CANARY_PHRASES.len() + 260);

    // Without the file, the configured value is back, cache or not.
    let plain = check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(plain.threshold_source, ThresholdSource::Configured);
    assert_eq!(plain.merge_threshold, 0.85);
}

#[tokio::test]
async fn fewer_than_a_hundred_pairs_keep_the_configured_threshold() {
    let dir = Scratch::new();
    let store = store();
    let options = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(write_pairs(dir.path(), MIN_RELATED_PAIRS - 1, "z")),
    };
    let out = check(&store, &Probe::new(0.0), &contract(), &options, 0.85)
        .await
        .unwrap();
    assert_eq!(out.threshold_source, ThresholdSource::Configured);
    assert_eq!(out.merge_threshold, 0.85);
}

#[tokio::test]
async fn unrelated_pairs_never_become_the_merge_threshold() {
    // 120 "pairs" of unrelated texts: their p99 sits at the random-pair
    // floor, which a merge threshold must clear (#176 correction).
    let dir = Scratch::new();
    let pairs: Vec<_> = (0..120)
        .map(|i| serde_json::json!({"a": format!("u{i}a"), "b": format!("u{i}b")}))
        .collect();
    let path = dir.path().join("unrelated.json");
    std::fs::write(&path, serde_json::to_vec(&pairs).unwrap()).unwrap();
    let store = store();
    let options = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(path),
    };
    let out = check(&store, &Probe::new(0.0), &contract(), &options, 0.85)
        .await
        .unwrap();
    assert_eq!(out.threshold_source, ThresholdSource::Configured, "{out:?}");
}

#[tokio::test]
async fn an_unreadable_pairs_file_is_a_configuration_error() {
    let store = store();
    let options = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some("/nonexistent/lambo-pairs.json".into()),
    };
    let err = check(&store, &Probe::new(0.0), &contract(), &options, 0.85)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("related_pairs"), "{err}");
}

#[test]
fn quantiles_are_nearest_rank() {
    let q = pair_quantiles((1..=100).map(f64::from).collect()).unwrap();
    assert_eq!((q.p50, q.p90, q.p99, q.max), (50.0, 90.0, 99.0, 100.0));
    let one = pair_quantiles(vec![0.3]).unwrap();
    assert_eq!((one.p50, one.p99), (0.3, 0.3));
    assert!(pair_quantiles(Vec::new()).is_none());
}

#[test]
fn modes_parse_and_print() {
    for m in [CanaryMode::Verify, CanaryMode::Warn, CanaryMode::Off] {
        assert_eq!(m.to_string().parse::<CanaryMode>().unwrap(), m);
    }
    assert_eq!(
        " VERIFY ".parse::<CanaryMode>().unwrap(),
        CanaryMode::Verify
    );
    assert!("strict".parse::<CanaryMode>().is_err());
    assert_eq!(CanaryOptions::default().mode, CanaryMode::Off);
    assert_eq!(
        crate::embed::EmbedderConfig::default().canary,
        CanaryMode::Warn
    );
}

/// The builder gate: `verify` refuses a drifted writer attach, `warn` and the
/// operator override attach.
#[tokio::test]
async fn verify_refuses_a_drifted_attach_and_warn_attaches() {
    let store: Arc<dyn GraphStore> = Arc::new(store());
    let attach = |drift: f64, mode: CanaryMode, allow: bool, session: &'static str| {
        let store = store.clone();
        async move {
            crate::Memory::builder()
                .session(session)
                .agent("a")
                .store(store)
                .embedder(Arc::new(Probe::new(drift)) as Arc<dyn Embedder>)
                .embedding_contract(contract())
                .canary(CanaryOptions {
                    mode,
                    related_pairs: None,
                })
                .allow_embedding_mismatch(allow)
                .build()
                .await
        }
    };
    let pin = attach(0.0, CanaryMode::Verify, false, "s1").await.unwrap();
    pin.close().await.unwrap();
    assert!(store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .is_some());

    let err = match attach(0.2, CanaryMode::Verify, false, "s2").await {
        Err(e) => e,
        Ok(_) => panic!("a drifted verify attach must refuse"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("canary drifted") && msg.contains("lambo re-embed"),
        "{msg}"
    );

    for (mode, allow, session) in [
        (CanaryMode::Warn, false, "s3"),
        (CanaryMode::Verify, true, "s4"),
        (CanaryMode::Off, false, "s5"),
    ] {
        let mem = attach(0.2, mode, allow, session).await.unwrap();
        mem.close().await.unwrap();
    }
}
