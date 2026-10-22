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
    // Verifying records when, and keeps the pinned vectors.
    let again: Fingerprint = serde_json::from_str(
        &store
            .read_store_meta(&fingerprint_key(&contract()))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(again.verified_at.is_some());
    assert_eq!(again.vectors, fp.vectors);
    assert_eq!(again.pinned_at, fp.pinned_at);
}

#[tokio::test]
async fn verify_mode_never_trusts_the_recheck_window() {
    // A daemon restarted on a swapped embedder inside the window attaches
    // once and then runs for days: under verify it must embed every time.
    let store = store();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    let verify = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: None,
    };
    let probe = Probe::new(0.0);
    check_at(&store, &probe, &contract(), &verify, 0.85, at(0))
        .await
        .unwrap();
    let first = check_at(&store, &probe, &contract(), &verify, 0.85, at(1))
        .await
        .unwrap();
    assert!(matches!(first.verdict, CanaryVerdict::Verified { .. }));
    let drifted = Probe::new(0.3);
    let out = check_at(&store, &drifted, &contract(), &verify, 0.85, at(60))
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Drifted { .. }),
        "{out:?}"
    );
    assert_eq!(drifted.calls(), CANARY_PHRASES.len());
}

#[tokio::test]
async fn a_recent_verification_is_trusted_and_an_old_one_is_not() {
    let store = store();
    let t0 = chrono::Utc::now();
    let probe = Probe::new(0.0);
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    check_at(&store, &probe, &contract(), &warn(), 0.85, at(0))
        .await
        .unwrap();
    // The first check after a pin always embeds.
    let first = check_at(&store, &probe, &contract(), &warn(), 0.85, at(1))
        .await
        .unwrap();
    assert!(matches!(first.verdict, CanaryVerdict::Verified { .. }));
    let calls = probe.calls();

    // Inside the window: one store read, no embed, even for a drifted
    // embedder. That is the documented trade.
    let drifted = Probe::new(0.3);
    let recent = check_at(&store, &drifted, &contract(), &warn(), 0.85, at(60))
        .await
        .unwrap();
    assert_eq!(recent.verdict, CanaryVerdict::RecentlyVerified);
    assert_eq!(drifted.calls(), 0);
    assert_eq!(probe.calls(), calls);

    // Past it, the drift is caught.
    let late = CANARY_RECHECK.as_secs() as i64 + 2;
    let out = check_at(&store, &drifted, &contract(), &warn(), 0.85, at(late))
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Drifted { .. }),
        "{out:?}"
    );
    // A clock that went backwards is not "recent" either.
    let back = check_at(&store, &drifted, &contract(), &warn(), 0.85, at(-60))
        .await
        .unwrap();
    assert!(
        matches!(back.verdict, CanaryVerdict::Drifted { .. }),
        "{back:?}"
    );
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

/// Answers every batch one vector short.
struct Short(Probe);

#[async_trait::async_trait]
impl Embedder for Short {
    fn dimensions(&self) -> usize {
        DIM
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.0.embed(text).await
    }
    fn batch_capacity(&self) -> usize {
        64
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
        _deadline: tokio::time::Instant,
    ) -> Vec<Result<Vec<f32>, EmbedError>> {
        let mut out = Vec::new();
        for t in &texts[..texts.len().saturating_sub(1)] {
            out.push(self.0.embed(t).await);
        }
        out
    }
}

#[tokio::test]
async fn a_short_batch_leaves_the_check_unverified_and_pins_nothing() {
    let store = store();
    let short = Short(Probe::new(0.0));
    let out = check(&store, &short, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert!(
        matches!(&out.verdict, CanaryVerdict::Unverified(why) if why.contains("19 vectors for 20")),
        "{out:?}"
    );
    assert!(store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn two_sessions_racing_the_first_pin_share_one_fingerprint() {
    // The second attach must compare against the winner's pin, not invent
    // another. Pre-seed the store with a pin, then a second check with a
    // drifted embedder must see Drifted (it lost any chance to pin).
    let store = store();
    let key = fingerprint_key(&contract());
    let probe = Probe::new(0.0);
    let first = check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert_eq!(first.verdict, CanaryVerdict::Pinned);
    let winner = store.read_store_meta(&key).await.unwrap().unwrap();

    // A second "first pin" path: clear is not available, so simulate the
    // race loser by calling write_store_meta_if_absent ourselves with a
    // different payload — it must refuse — then check with a drifted probe.
    let inserted = store
        .write_store_meta_if_absent(&key, "should-not-land")
        .await
        .unwrap();
    assert!(!inserted);
    assert_eq!(
        store.read_store_meta(&key).await.unwrap().as_deref(),
        Some(winner.as_str())
    );

    let drifted = Probe::new(0.3);
    let out = check(&store, &drifted, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    assert!(
        matches!(out.verdict, CanaryVerdict::Drifted { .. }),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_too_short_related_pairs_file_is_cached_for_the_recheck_window() {
    let dir = Scratch::new();
    let path = dir.0.join("pairs.json");
    // Fewer than MIN_RELATED_PAIRS.
    let pairs: Vec<_> = (0..10)
        .map(|i| serde_json::json!({"a": format!("rel:{i}:a"), "b": format!("rel:{i}:b")}))
        .collect();
    std::fs::write(&path, serde_json::to_string(&pairs).unwrap()).unwrap();
    let opts = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(path),
    };
    let store = store();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    let probe = Probe::new(0.0);
    let first = check_at(&store, &probe, &contract(), &opts, 0.85, at(0))
        .await
        .unwrap();
    assert_eq!(first.verdict, CanaryVerdict::Pinned);
    assert_eq!(first.merge_threshold, 0.85);
    let verified = check_at(&store, &probe, &contract(), &opts, 0.85, at(1))
        .await
        .unwrap();
    assert!(matches!(verified.verdict, CanaryVerdict::Verified { .. }));
    let calls = probe.calls();
    // Inside the window the unusable cache must still short-circuit.
    let recent = check_at(&store, &probe, &contract(), &opts, 0.85, at(60))
        .await
        .unwrap();
    assert_eq!(recent.verdict, CanaryVerdict::RecentlyVerified);
    assert_eq!(probe.calls(), calls);
}

/// #191: answers the canaries, but fails every labelled-pair text while
/// `pairs_down` is set (an embedder that is down or slow for the large file).
struct PairsDown {
    probe: Probe,
    pairs_down: std::sync::atomic::AtomicBool,
    /// How long a failing pair text hangs first (a slow embedder).
    hang: Duration,
}

impl PairsDown {
    fn new() -> Self {
        Self {
            probe: Probe::new(0.0),
            pairs_down: std::sync::atomic::AtomicBool::new(true),
            hang: Duration::ZERO,
        }
    }
    fn recover(&self) {
        self.pairs_down.store(false, Ordering::SeqCst);
    }
    fn calls(&self) -> usize {
        self.probe.calls()
    }
}

#[async_trait::async_trait]
impl Embedder for PairsDown {
    fn dimensions(&self) -> usize {
        DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.starts_with("rel:") && self.pairs_down.load(Ordering::SeqCst) {
            self.probe.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.hang).await;
            return Err(EmbedError::Unavailable("pairs embed timed out".into()));
        }
        self.probe.embed(text).await
    }
}

async fn stored_fingerprint(store: &crate::MemoryStore) -> Fingerprint {
    let raw = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .expect("pinned");
    serde_json::from_str(&raw).unwrap()
}

// Paused: the wait is counted from the failure, so the attempt must take
// no time for the deadlines below to be exact.
#[tokio::test(start_paused = true)]
async fn failing_related_pairs_back_off_and_are_measured_once_the_embedder_recovers() {
    let dir = Scratch::new();
    let opts = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(write_pairs(dir.path(), 120, "b")),
    };
    let store = store();
    let embedder = PairsDown::new();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);

    // Attach 1: pins, fails the pairs, records a retry.
    let first = check_at(&store, &embedder, &contract(), &opts, 0.85, at(0))
        .await
        .unwrap();
    assert_eq!(first.verdict, CanaryVerdict::Pinned);
    assert_eq!(first.threshold_source, ThresholdSource::Configured);
    let retry = stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .expect("a retry is recorded");
    assert_eq!(retry.failures, 1);
    assert_eq!(retry.next_attempt_at, at(60));
    assert!(
        retry.last_error.contains("timed out"),
        "{}",
        retry.last_error
    );

    // Attach 2, inside the backoff: the canaries verify, the pairs are not
    // embedded again.
    let before = embedder.calls();
    let second = check_at(&store, &embedder, &contract(), &opts, 0.85, at(1))
        .await
        .unwrap();
    assert!(matches!(second.verdict, CanaryVerdict::Verified { .. }));
    assert_eq!(second.merge_threshold, 0.85);
    assert_eq!(embedder.calls() - before, CANARY_PHRASES.len());

    // Attach 3, inside the backoff and the recheck window: nothing embedded.
    let before = embedder.calls();
    let third = check_at(&store, &embedder, &contract(), &opts, 0.85, at(2))
        .await
        .unwrap();
    assert_eq!(third.verdict, CanaryVerdict::RecentlyVerified);
    assert_eq!(third.merge_threshold, 0.85);
    assert_eq!(embedder.calls(), before);

    // Attach 4, after next_attempt_at, with the embedder back: measured,
    // cached, and the retry cleared.
    embedder.recover();
    let before = embedder.calls();
    let fourth = check_at(&store, &embedder, &contract(), &opts, 0.85, at(60))
        .await
        .unwrap();
    assert_eq!(
        fourth.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 120 }
    );
    assert_eq!(embedder.calls() - before, CANARY_PHRASES.len() + 240);
    let fp = stored_fingerprint(&store).await;
    assert!(fp.related_pairs_retry.is_none());
    assert_eq!(fp.merge_threshold.unwrap().pairs, 120);
}

#[test]
fn the_pairs_backoff_doubles_up_to_the_cap() {
    let secs: Vec<u64> = (1..=9).map(|n| PairsRetry::delay(n).as_secs()).collect();
    assert_eq!(secs, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
    assert_eq!(PairsRetry::delay(0), PAIRS_RETRY_BASE);
    assert_eq!(PairsRetry::delay(u32::MAX), PAIRS_RETRY_CAP);
}

// Paused: the wait is counted from the failure, so the attempt must take
// no time for the deadlines below to be exact.
#[tokio::test(start_paused = true)]
async fn repeated_pairs_failures_grow_the_wait() {
    let dir = Scratch::new();
    let opts = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: Some(write_pairs(dir.path(), 120, "g")),
    };
    let store = store();
    let embedder = PairsDown::new();
    let t0 = chrono::Utc::now();
    let mut now = t0;
    for (failures, wait) in [(1, 60), (2, 120), (3, 240), (4, 480)] {
        check_at(&store, &embedder, &contract(), &opts, 0.85, now)
            .await
            .unwrap();
        let retry = stored_fingerprint(&store)
            .await
            .related_pairs_retry
            .unwrap();
        assert_eq!(retry.failures, failures);
        assert_eq!(retry.next_attempt_at, now + chrono::Duration::seconds(wait));
        // One second short of the wait: still backing off, so the count
        // does not move.
        check_at(
            &store,
            &embedder,
            &contract(),
            &opts,
            0.85,
            retry.next_attempt_at - chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
        assert_eq!(
            stored_fingerprint(&store)
                .await
                .related_pairs_retry
                .unwrap()
                .failures,
            failures
        );
        now = retry.next_attempt_at;
    }
}

#[tokio::test]
async fn a_changed_pairs_file_is_measured_during_a_backoff() {
    let dir = Scratch::new();
    let store = store();
    let embedder = PairsDown::new();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    let old = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(write_pairs(dir.path(), 120, "o")),
    };
    check_at(&store, &embedder, &contract(), &old, 0.85, at(0))
        .await
        .unwrap();
    check_at(&store, &embedder, &contract(), &old, 0.85, at(1))
        .await
        .unwrap();
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_some());

    // Inside both the backoff and the recheck window, a new file is
    // measured, not trusted.
    embedder.recover();
    let new = CanaryOptions {
        related_pairs: Some(write_pairs(dir.path(), 110, "n")),
        ..old.clone()
    };
    let before = embedder.calls();
    let out = check_at(&store, &embedder, &contract(), &new, 0.85, at(2))
        .await
        .unwrap();
    assert_eq!(
        out.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 110 }
    );
    assert_eq!(embedder.calls() - before, CANARY_PHRASES.len() + 220);
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_none());
}

#[tokio::test]
async fn a_new_file_that_fails_starts_its_own_count_and_never_uses_the_old_threshold() {
    let dir = Scratch::new();
    let store = store();
    let embedder = PairsDown::new();
    embedder.recover();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    let old = CanaryOptions {
        mode: CanaryMode::Warn,
        related_pairs: Some(write_pairs(dir.path(), 120, "p")),
    };
    let first = check_at(&store, &embedder, &contract(), &old, 0.85, at(0))
        .await
        .unwrap();
    assert_eq!(
        first.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 120 }
    );
    check_at(&store, &embedder, &contract(), &old, 0.85, at(1))
        .await
        .unwrap();

    // The file changes and the embedder goes down for the pairs.
    embedder.pairs_down.store(true, Ordering::SeqCst);
    let new = CanaryOptions {
        related_pairs: Some(write_pairs(dir.path(), 130, "q")),
        ..old.clone()
    };
    let failed = check_at(&store, &embedder, &contract(), &new, 0.85, at(2))
        .await
        .unwrap();
    assert_eq!(failed.threshold_source, ThresholdSource::Configured);
    let fp = stored_fingerprint(&store).await;
    assert_eq!(fp.related_pairs_retry.as_ref().unwrap().failures, 1);
    // The old file's derivation is kept, but never applied to the new file,
    // also on the recheck-window shortcut.
    assert_eq!(fp.merge_threshold.unwrap().pairs, 120);
    let recent = check_at(&store, &embedder, &contract(), &new, 0.85, at(3))
        .await
        .unwrap();
    assert_eq!(recent.verdict, CanaryVerdict::RecentlyVerified);
    assert_eq!(recent.threshold_source, ThresholdSource::Configured);
    assert_eq!(recent.merge_threshold, 0.85);
}

#[tokio::test]
async fn verify_still_embeds_the_canaries_during_a_pairs_backoff() {
    let dir = Scratch::new();
    let opts = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: Some(write_pairs(dir.path(), 120, "v")),
    };
    let store = store();
    let embedder = PairsDown::new();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    check_at(&store, &embedder, &contract(), &opts, 0.85, at(0))
        .await
        .unwrap();
    for secs in [1, 2, 30] {
        let before = embedder.calls();
        let out = check_at(&store, &embedder, &contract(), &opts, 0.85, at(secs))
            .await
            .unwrap();
        assert!(matches!(out.verdict, CanaryVerdict::Verified { .. }));
        assert_eq!(embedder.calls() - before, CANARY_PHRASES.len());
    }
}

#[test]
fn a_backoff_the_clock_moved_back_past_is_not_honoured() {
    let now = chrono::Utc::now();
    let retry = PairsRetry {
        source_sha256: "s".into(),
        failures: 7,
        next_attempt_at: now + chrono::Duration::from_std(PAIRS_RETRY_CAP).unwrap(),
        last_error: String::new(),
    };
    assert!(retry.holds("s", now));
    assert!(!retry.holds("other", now));
    assert!(!retry.holds("s", now - chrono::Duration::seconds(1)));
    assert!(!retry.holds("s", retry.next_attempt_at));
}

#[tokio::test]
async fn a_pin_written_without_the_retry_field_still_decodes() {
    let store = store();
    let probe = Probe::new(0.0);
    check(&store, &probe, &contract(), &warn(), 0.85)
        .await
        .unwrap();
    let raw = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .unwrap();
    // A pin with no retry writes no field, as 0.7.0 did.
    assert!(!raw.contains("related_pairs_retry"), "{raw}");
    let fp: Fingerprint = serde_json::from_str(&raw).unwrap();
    assert!(fp.related_pairs_retry.is_none());
    // And with one set, it round-trips.
    let with = Fingerprint {
        related_pairs_retry: Some(PairsRetry {
            source_sha256: "abc".into(),
            failures: 2,
            next_attempt_at: chrono::Utc::now(),
            last_error: "down".into(),
        }),
        ..fp
    };
    let back: Fingerprint = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
    assert_eq!(back, with);
}

#[tokio::test]
async fn a_retry_for_another_file_is_dropped() {
    let dir = Scratch::new();
    let store = store();
    let embedder = PairsDown::new();
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    let failing = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: Some(write_pairs(dir.path(), 120, "f")),
    };
    check_at(&store, &embedder, &contract(), &failing, 0.85, at(0))
        .await
        .unwrap();
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_some());

    // A too-short file (cached as unusable, nothing embedded for it) drops
    // the other file's retry, so going back to the failing file measures it.
    let short = CanaryOptions {
        related_pairs: Some(write_pairs(dir.path(), 10, "s")),
        ..failing.clone()
    };
    check_at(&store, &embedder, &contract(), &short, 0.85, at(1))
        .await
        .unwrap();
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_none());

    embedder.recover();
    let back = check_at(&store, &embedder, &contract(), &failing, 0.85, at(2))
        .await
        .unwrap();
    assert_eq!(
        back.threshold_source,
        ThresholdSource::RelatedPairs { pairs: 120 }
    );

    // No file at all drops a retry too.
    embedder.pairs_down.store(true, Ordering::SeqCst);
    let other = CanaryOptions {
        related_pairs: Some(write_pairs(dir.path(), 120, "h")),
        ..failing.clone()
    };
    check_at(&store, &embedder, &contract(), &other, 0.85, at(3))
        .await
        .unwrap();
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_some());
    let none = CanaryOptions {
        related_pairs: None,
        ..failing
    };
    check_at(&store, &embedder, &contract(), &none, 0.85, at(4))
        .await
        .unwrap();
    assert!(stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .is_none());
}

// Paused: the hang advances tokio's clock, not the test's wall time.
#[tokio::test(start_paused = true)]
async fn a_slow_pairs_failure_still_waits_the_full_delay() {
    let dir = Scratch::new();
    let opts = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: Some(write_pairs(dir.path(), 120, "w")),
    };
    let store = store();
    // Hangs past the budget: the attempt fails at the deadline, 60 s in.
    let embedder = PairsDown {
        hang: Duration::from_secs(90),
        ..PairsDown::new()
    };
    let t0 = chrono::Utc::now();
    let at = |secs: i64| t0 + chrono::Duration::seconds(secs);
    check_at(&store, &embedder, &contract(), &opts, 0.85, at(0))
        .await
        .unwrap();
    let retry = stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .unwrap();
    let budget = CANARY_EMBED_TIMEOUT.as_secs() as i64;
    assert_eq!(retry.next_attempt_at, at(budget + 60));

    // Past `now + delay` but inside the wait counted from the failure: the
    // pairs are not tried again.
    let before = embedder.calls();
    check_at(&store, &embedder, &contract(), &opts, 0.85, at(budget + 59))
        .await
        .unwrap();
    assert_eq!(embedder.calls() - before, CANARY_PHRASES.len());
}

#[tokio::test(start_paused = true)]
async fn a_pairs_failure_keeps_no_pair_text_in_the_pin() {
    let dir = Scratch::new();
    let opts = CanaryOptions {
        mode: CanaryMode::Verify,
        related_pairs: Some(write_pairs(dir.path(), 120, "secret")),
    };
    let store = store();
    let embedder = PairsDown::new();
    check_at(
        &store,
        &embedder,
        &contract(),
        &opts,
        0.85,
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let raw = store
        .read_store_meta(&fingerprint_key(&contract()))
        .await
        .unwrap()
        .unwrap();
    assert!(raw.contains("related_pairs_retry"), "{raw}");
    assert!(!raw.contains("rel:secret"), "{raw}");
    let retry = stored_fingerprint(&store)
        .await
        .related_pairs_retry
        .unwrap();
    assert!(
        retry.last_error.starts_with("embedding text 1 of 240"),
        "{}",
        retry.last_error
    );
}
