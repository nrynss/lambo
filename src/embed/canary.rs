//! #176: the embedder canary fingerprint.
//!
//! An [`EmbeddingContract`] is `{kind, model, dim}`, compared as strings. A
//! change behind the same name (a re-quantised GGUF, another llama.cpp build,
//! a provider's model revision) passes that check and mixes two vector spaces
//! in one store. The canary catches it: [`CANARY_PHRASES`] are embedded once
//! when the store is first opened under a contract, the vectors are kept in
//! `store_meta` (key [`fingerprint_key`]), and every later writer attach
//! embeds them again. Any phrase whose cosine against its stored vector is
//! below [`CANARY_MIN_COSINE`] means the embedder no longer produces the space
//! the store was written in.
//!
//! What a failed check does is `[embedder] canary` ([`CanaryMode`]):
//! `verify` refuses the writer attach (fail closed) unless the operator passed
//! the embedding-mismatch override, `warn` logs and attaches, `off` skips the
//! check. Reads that do not take the writer lease are not gated.
//!
//! The fingerprint is per store, not per session: every session in a store
//! shares its embedder, so a per-session copy could only drift or disagree.
//! It is keyed by the contract, so a store holding sessions under two
//! contracts keeps one fingerprint for each.
//!
//! The fingerprint also records the quantiles of the cosine between pairs of
//! *different* canary phrases ([`PairQuantiles`]). Those are unrelated texts,
//! so this is a relevance floor and nothing more. It is never a merge
//! threshold: under lambo's framing unrelated pairs sit near 0.5–0.62 on
//! BGE-M3, and a merge threshold there would merge every related-but-distinct
//! pair (#176, #87).
//!
//! The merge threshold may instead be derived per #87: the p99 cosine of
//! operator-labelled related-but-distinct pairs (`[embedder] related_pairs`,
//! a JSON array of `{"a": .., "b": ..}`), with at least
//! [`MIN_RELATED_PAIRS`] of them. With fewer, or none, the configured
//! `semantic_match_threshold` stays. The derived value is cached in the
//! fingerprint against the file's SHA-256, so the pairs are embedded again
//! only when the file changes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Embedder;
use crate::store::GraphStore;
use crate::types::{EmbeddingContract, LamboError};

/// Bump when [`CANARY_PHRASES`] change: a fingerprint of another version is
/// pinned again rather than compared.
pub const CANARY_VERSION: u32 = 1;

/// Below this cosine against its stored vector, a canary phrase has drifted
/// (#176: fail closed below 0.99).
pub const CANARY_MIN_COSINE: f64 = 0.99;

/// #87: the fewest labelled related-but-distinct pairs a derived merge
/// threshold may rest on.
pub const MIN_RELATED_PAIRS: usize = 100;

/// Per-call bound on the canary embeds, so a wedged embedder cannot hold a
/// startup for ever. A timeout leaves the check unverified, not failed.
pub const CANARY_EMBED_TIMEOUT: Duration = Duration::from_secs(60);

/// A fingerprint verified this recently is trusted without embedding the
/// canaries again, so a burst of one-shot CLI writers (each a writer attach)
/// costs one store read each, not twenty embeds. A pin is never trusted this
/// way: the first attach after it always verifies.
pub const CANARY_RECHECK: Duration = Duration::from_secs(10 * 60);

/// The fixed canary set: mixed domains and languages, prose, code,
/// identifiers and numbers. Versioned by [`CANARY_VERSION`].
pub const CANARY_PHRASES: [&str; 20] = [
    "The quarterly invoice was paid late because the bank holiday moved the cutoff.",
    "fn parse_config(path: &Path) -> Result<Config, Error>",
    "Photosynthesis converts light energy into chemical energy stored in glucose.",
    "Retry the POST /v1/orders request with exponential backoff and jitter.",
    "The river flooded the lower town twice during the spring of 1927.",
    "SELECT id, name FROM users WHERE created_at > now() - interval '7 days'",
    "A minor key often sounds sad, while a major key sounds bright.",
    "Kubernetes evicted the pod after it exceeded its memory limit of 512Mi.",
    "She preferred tea over coffee and always added a slice of lemon.",
    "ERR_CONNECTION_REFUSED on localhost:8080 after the container restarted",
    "The patient reported mild headaches and was advised to drink more water.",
    "Use a mutex to guard the shared counter across worker threads.",
    "Le chat dort sur le canapé pendant que la pluie tombe dehors.",
    "The offside rule applies when an attacker is behind the last defender.",
    "git rebase --onto main feature~3 feature",
    "Compound interest grows savings faster than simple interest over decades.",
    "The migration adds a NOT NULL column with a default to avoid a table rewrite.",
    "Basalt forms when lava cools quickly at the surface.",
    "Ship the hotfix behind the LAMBO_SAFE_MODE feature flag first.",
    "東京の天気は明日晴れるでしょう。",
];

/// `[embedder] canary`: what a failed canary check does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CanaryMode {
    /// Refuse the writer attach when the canary drifted (fail closed).
    Verify,
    /// Log the drift and attach anyway. The default for this release.
    #[default]
    Warn,
    /// No canary: nothing is embedded, pinned or compared.
    Off,
}

impl std::str::FromStr for CanaryMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "verify" => Ok(Self::Verify),
            "warn" => Ok(Self::Warn),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "unknown canary mode {other:?} (expected verify, warn or off)"
            )),
        }
    }
}

impl std::fmt::Display for CanaryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Verify => "verify",
            Self::Warn => "warn",
            Self::Off => "off",
        })
    }
}

/// What the writer attach runs: the mode and the optional labelled pairs.
/// The default is [`CanaryOptions::off`]: a hand-assembled builder runs no
/// check; `[embedder] canary` defaults to `warn` on the resolved path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanaryOptions {
    pub mode: CanaryMode,
    /// `[embedder] related_pairs`: labelled related-but-distinct pairs for a
    /// #87 merge threshold.
    pub related_pairs: Option<PathBuf>,
}

impl Default for CanaryOptions {
    fn default() -> Self {
        Self::off()
    }
}

impl CanaryOptions {
    /// No check at all: what a builder without `.backends(..)` runs.
    pub fn off() -> Self {
        Self {
            mode: CanaryMode::Off,
            related_pairs: None,
        }
    }
}

/// Cosine quantiles over pairs of texts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairQuantiles {
    pub pairs: usize,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
}

/// A merge threshold derived per #87, and the file it came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DerivedThreshold {
    /// p99 cosine of the labelled related-but-distinct pairs.
    pub value: f64,
    pub pairs: usize,
    /// SHA-256 (hex) of the pairs file, so a changed file is measured again.
    pub source_sha256: String,
}

/// What `store_meta` keeps for one contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub version: u32,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub dim: usize,
    pub pinned_at: chrono::DateTime<chrono::Utc>,
    /// One base64 packed little-endian f32 vector per [`CANARY_PHRASES`] entry.
    pub vectors: Vec<String>,
    /// Cosine between different canary phrases: a relevance floor only,
    /// never a merge threshold (module docs).
    pub random_pair: PairQuantiles,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_threshold: Option<DerivedThreshold>,
    /// The last attach that embedded the canaries and found them within
    /// [`CANARY_MIN_COSINE`]. `None` until the first verification after a pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Fingerprint {
    fn decoded(&self) -> Option<Vec<Vec<f32>>> {
        self.vectors
            .iter()
            .map(|v| {
                let bytes = base64::engine::general_purpose::STANDARD.decode(v).ok()?;
                let (chunks, rest) = bytes.as_chunks::<4>();
                if !rest.is_empty() || chunks.len() != self.dim {
                    return None;
                }
                Some(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
            })
            .collect()
    }
}

/// How the attach's check went.
#[derive(Clone, Debug, PartialEq)]
pub enum CanaryVerdict {
    /// `canary = "off"`, or a re-embed attach.
    Off,
    /// No fingerprint (or one of another canary version): this attach pinned it.
    Pinned,
    /// Every phrase is at or above [`CANARY_MIN_COSINE`].
    Verified { min_cosine: f64, worst: usize },
    /// Verified less than [`CANARY_RECHECK`] ago; nothing was embedded.
    RecentlyVerified,
    /// At least one phrase is below [`CANARY_MIN_COSINE`].
    Drifted { min_cosine: f64, worst: usize },
    /// The canaries could not be embedded (the embedder is down or slow).
    /// Not a measured drift, so never a refusal; the next attach checks again.
    Unverified(String),
}

/// Which merge threshold the attach runs with.
#[derive(Clone, Debug, PartialEq)]
pub enum ThresholdSource {
    /// `semantic_match_threshold` as configured.
    Configured,
    /// #87: the p99 of this many labelled related-but-distinct pairs.
    RelatedPairs { pairs: usize },
}

/// The result of [`check`].
#[derive(Clone, Debug, PartialEq)]
pub struct CanaryOutcome {
    pub verdict: CanaryVerdict,
    pub merge_threshold: f64,
    pub threshold_source: ThresholdSource,
}

/// The `store_meta` key a contract's fingerprint lives under.
pub fn fingerprint_key(contract: &EmbeddingContract) -> String {
    format!(
        "embedder_fingerprint:{}:{}:{}",
        contract.kind,
        contract.model.as_deref().unwrap_or(""),
        contract.dim
    )
}

/// One labelled pair in the `related_pairs` file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedPair {
    pub a: String,
    pub b: String,
}

/// Cosine in f64: the decision sits at 0.99, so the accumulation must not
/// add f32 rounding of its own.
pub fn cosine64(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Nearest-rank quantile of an ascending slice.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Quantiles of the given cosines. `None` when there are none.
pub fn pair_quantiles(mut cosines: Vec<f64>) -> Option<PairQuantiles> {
    if cosines.is_empty() {
        return None;
    }
    cosines.sort_by(f64::total_cmp);
    Some(PairQuantiles {
        pairs: cosines.len(),
        p50: quantile(&cosines, 0.50),
        p90: quantile(&cosines, 0.90),
        p99: quantile(&cosines, 0.99),
        max: *cosines.last().expect("non-empty"),
    })
}

/// Cosine between every pair of different canary vectors.
fn random_pair_quantiles(vectors: &[Vec<f32>]) -> PairQuantiles {
    let mut cosines = Vec::with_capacity(vectors.len() * vectors.len() / 2);
    for (i, a) in vectors.iter().enumerate() {
        for b in &vectors[i + 1..] {
            cosines.push(cosine64(a, b));
        }
    }
    pair_quantiles(cosines).expect("20 canaries make 190 pairs")
}

/// Embed `texts` in order, every vector `dim` wide.
async fn embed_all(
    embedder: &dyn Embedder,
    texts: &[&str],
    dim: usize,
) -> Result<Vec<Vec<f32>>, String> {
    let deadline = tokio::time::Instant::now() + CANARY_EMBED_TIMEOUT;
    let mut out = Vec::with_capacity(texts.len());
    let cap = embedder.batch_capacity().max(1);
    for chunk in texts.chunks(cap) {
        for (text, vector) in chunk
            .iter()
            .zip(embedder.embed_batch(chunk, deadline).await)
        {
            let vector = vector.map_err(|e| format!("embedding {text:?}: {e}"))?;
            if vector.len() != dim {
                return Err(format!(
                    "embedding {text:?}: {} values, the contract says {dim}",
                    vector.len()
                ));
            }
            out.push(vector);
        }
    }
    Ok(out)
}

fn pack(vector: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for x in vector {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn store_err(what: &str, e: impl std::fmt::Display) -> LamboError {
    LamboError::Config(format!("embedder canary: {what}: {e}"))
}

/// A fresh fingerprint from freshly embedded canaries.
fn fingerprint_of(contract: &EmbeddingContract, vectors: &[Vec<f32>]) -> Fingerprint {
    Fingerprint {
        version: CANARY_VERSION,
        kind: contract.kind.clone(),
        model: contract.model.clone(),
        dim: contract.dim,
        pinned_at: chrono::Utc::now(),
        vectors: vectors.iter().map(|v| pack(v)).collect(),
        random_pair: random_pair_quantiles(vectors),
        merge_threshold: None,
        verified_at: None,
    }
}

async fn write_fingerprint(
    store: &dyn GraphStore,
    key: &str,
    fingerprint: &Fingerprint,
) -> Result<(), LamboError> {
    let value = serde_json::to_string(fingerprint).map_err(|e| store_err("encode", e))?;
    store
        .write_store_meta(key, &value)
        .await
        .map_err(LamboError::Store)
}

/// Embed the canaries and pin them as this contract's fingerprint,
/// replacing any earlier one (`lambo re-embed` after a deliberate model
/// change). A cached #87 threshold is dropped with the old vectors.
pub async fn pin(
    store: &dyn GraphStore,
    embedder: &dyn Embedder,
    contract: &EmbeddingContract,
) -> Result<(), LamboError> {
    let vectors = embed_all(embedder, &CANARY_PHRASES, contract.dim)
        .await
        .map_err(LamboError::EmbedUnavailable)?;
    write_fingerprint(
        store,
        &fingerprint_key(contract),
        &fingerprint_of(contract, &vectors),
    )
    .await
}

/// Read and parse the labelled pairs file.
fn read_related_pairs(path: &Path) -> Result<(Vec<RelatedPair>, String), LamboError> {
    let bytes = std::fs::read(path).map_err(|e| {
        LamboError::Config(format!(
            "[embedder] related_pairs {}: cannot read it: {e}",
            path.display()
        ))
    })?;
    let sha = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let pairs: Vec<RelatedPair> = serde_json::from_slice(&bytes).map_err(|e| {
        LamboError::Config(format!(
            "[embedder] related_pairs {}: expected a JSON array of {{\"a\": .., \"b\": ..}}: {e}",
            path.display()
        ))
    })?;
    Ok((pairs, sha))
}

/// #87: the p99 cosine of the labelled pairs, embedded now.
async fn derive_threshold(
    embedder: &dyn Embedder,
    contract: &EmbeddingContract,
    pairs: &[RelatedPair],
    sha: String,
) -> Result<DerivedThreshold, String> {
    let texts: Vec<&str> = pairs
        .iter()
        .flat_map(|p| [p.a.as_str(), p.b.as_str()])
        .collect();
    let vectors = embed_all(embedder, &texts, contract.dim).await?;
    let cosines = vectors
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[a, b]| cosine64(a, b))
        .collect();
    let q = pair_quantiles(cosines).ok_or("no pairs")?;
    Ok(DerivedThreshold {
        value: q.p99,
        pairs: pairs.len(),
        source_sha256: sha,
    })
}

/// The threshold an attach runs with, given the (possibly updated) cached
/// derivation and the random-pair floor it must sit above.
fn effective_threshold(
    derived: Option<&DerivedThreshold>,
    floor: Option<&PairQuantiles>,
    configured: f64,
) -> (f64, ThresholdSource) {
    let Some(d) = derived else {
        return (configured, ThresholdSource::Configured);
    };
    let above_floor = floor.is_none_or(|f| d.value > f.p99);
    if d.pairs >= MIN_RELATED_PAIRS
        && d.value.is_finite()
        && (0.0..=1.0).contains(&d.value)
        && above_floor
    {
        (d.value, ThresholdSource::RelatedPairs { pairs: d.pairs })
    } else {
        tracing::warn!(
            derived = d.value,
            pairs = d.pairs,
            random_pair_p99 = floor.map(|f| f.p99),
            configured,
            "the related-pairs merge threshold is not usable (fewer than {MIN_RELATED_PAIRS} \
             pairs, or not above the random-pair floor); keeping semantic_match_threshold"
        );
        (configured, ThresholdSource::Configured)
    }
}

/// The writer attach's check: pin, verify or skip the canary, and settle the
/// merge threshold. Returns `Err` only for a store or configuration failure;
/// a drift is [`CanaryVerdict::Drifted`] and the caller applies the mode.
pub async fn check(
    store: &dyn GraphStore,
    embedder: &dyn Embedder,
    contract: &EmbeddingContract,
    options: &CanaryOptions,
    configured_threshold: f64,
) -> Result<CanaryOutcome, LamboError> {
    check_at(
        store,
        embedder,
        contract,
        options,
        configured_threshold,
        chrono::Utc::now(),
    )
    .await
}

/// [`check`] at a given time (tests drive the [`CANARY_RECHECK`] window).
pub(crate) async fn check_at(
    store: &dyn GraphStore,
    embedder: &dyn Embedder,
    contract: &EmbeddingContract,
    options: &CanaryOptions,
    configured_threshold: f64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<CanaryOutcome, LamboError> {
    let configured = CanaryOutcome {
        verdict: CanaryVerdict::Off,
        merge_threshold: configured_threshold,
        threshold_source: ThresholdSource::Configured,
    };
    if options.mode == CanaryMode::Off {
        return Ok(configured);
    }
    let key = fingerprint_key(contract);
    let stored = store
        .read_store_meta(&key)
        .await
        .map_err(LamboError::Store)?;
    let stored: Option<Fingerprint> = match stored {
        None => None,
        Some(raw) => match serde_json::from_str::<Fingerprint>(&raw) {
            Ok(f) if f.version == CANARY_VERSION && f.decoded().is_some() => Some(f),
            Ok(f) => {
                tracing::info!(
                    stored_version = f.version,
                    version = CANARY_VERSION,
                    "embedder canary set changed or the stored vectors are malformed; pinning \
                     it again"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "stored embedder fingerprint is unreadable; pinning it again");
                None
            }
        },
    };
    let related = options
        .related_pairs
        .as_deref()
        .map(read_related_pairs)
        .transpose()?;

    let cached_sha_matches = |f: &Fingerprint| match (&related, &f.merge_threshold) {
        (None, _) => true,
        (Some((_, sha)), Some(d)) => *sha == d.source_sha256,
        (Some(_), None) => false,
    };
    if let Some(f) = &stored {
        let recent = f.verified_at.is_some_and(|at| {
            now >= at && (now - at).to_std().is_ok_and(|age| age < CANARY_RECHECK)
        });
        if recent && cached_sha_matches(f) {
            let derived = f.merge_threshold.as_ref().filter(|_| related.is_some());
            let (merge_threshold, threshold_source) =
                effective_threshold(derived, Some(&f.random_pair), configured_threshold);
            return Ok(CanaryOutcome {
                verdict: CanaryVerdict::RecentlyVerified,
                merge_threshold,
                threshold_source,
            });
        }
    }

    let live = match embed_all(embedder, &CANARY_PHRASES, contract.dim).await {
        Ok(v) => v,
        Err(e) => {
            // Use what is cached, without re-measuring anything.
            let derived = stored.as_ref().and_then(|f| f.merge_threshold.as_ref());
            let derived = derived.filter(|d| {
                related
                    .as_ref()
                    .is_some_and(|(_, sha)| *sha == d.source_sha256)
            });
            let (merge_threshold, threshold_source) = effective_threshold(
                derived,
                stored.as_ref().map(|f| &f.random_pair),
                configured_threshold,
            );
            return Ok(CanaryOutcome {
                verdict: CanaryVerdict::Unverified(e),
                merge_threshold,
                threshold_source,
            });
        }
    };

    let (mut fingerprint, verdict) = match stored {
        None => (fingerprint_of(contract, &live), CanaryVerdict::Pinned),
        Some(f) => {
            let pinned = f.decoded().expect("checked above");
            let (worst, min_cosine) = pinned
                .iter()
                .zip(&live)
                .map(|(p, l)| cosine64(p, l))
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .expect("20 canaries");
            let verdict = if min_cosine < CANARY_MIN_COSINE {
                CanaryVerdict::Drifted { min_cosine, worst }
            } else {
                CanaryVerdict::Verified { min_cosine, worst }
            };
            (f, verdict)
        }
    };
    if matches!(verdict, CanaryVerdict::Drifted { .. }) {
        // A drifted embedder measures nothing worth caching: keep the old
        // fingerprint and the configured threshold.
        return Ok(CanaryOutcome {
            verdict,
            ..configured
        });
    }

    let mut dirty = verdict == CanaryVerdict::Pinned;
    if matches!(verdict, CanaryVerdict::Verified { .. }) {
        fingerprint.verified_at = Some(now);
        dirty = true;
    }
    match &related {
        None => {}
        Some((pairs, sha)) => {
            let cached = fingerprint
                .merge_threshold
                .as_ref()
                .is_some_and(|d| d.source_sha256 == *sha);
            if !cached {
                if pairs.len() < MIN_RELATED_PAIRS {
                    tracing::warn!(
                        pairs = pairs.len(),
                        "[embedder] related_pairs has fewer than {MIN_RELATED_PAIRS} pairs; \
                         keeping semantic_match_threshold"
                    );
                    fingerprint.merge_threshold = None;
                } else {
                    match derive_threshold(embedder, contract, pairs, sha.clone()).await {
                        Ok(d) => {
                            tracing::info!(
                                threshold = d.value,
                                pairs = d.pairs,
                                random_pair_p99 = fingerprint.random_pair.p99,
                                "derived the merge threshold from the related pairs (#87)"
                            );
                            fingerprint.merge_threshold = Some(d);
                            dirty = true;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "could not embed the related pairs; keeping semantic_match_threshold");
                        }
                    }
                }
            }
        }
    }
    if dirty {
        write_fingerprint(store, &key, &fingerprint).await?;
    }
    let derived = fingerprint.merge_threshold.as_ref().filter(|d| {
        related
            .as_ref()
            .is_some_and(|(_, sha)| *sha == d.source_sha256)
    });
    let (merge_threshold, threshold_source) = effective_threshold(
        derived,
        Some(&fingerprint.random_pair),
        configured_threshold,
    );
    Ok(CanaryOutcome {
        verdict,
        merge_threshold,
        threshold_source,
    })
}

#[cfg(all(test, feature = "store-memory"))]
#[path = "canary_tests.rs"]
mod tests;
