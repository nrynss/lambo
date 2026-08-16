//! Runtime configuration — named defaults from the v0.1 hackathon spec,
//! plus Level B process file (`lambo.toml`) for store/embedder selection.
//!
//! See `dev-diary/notes/level-b-pluggability.md`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::embed::{EmbedError, EmbedderConfig};
use crate::store::StoreConfig;
use crate::types::{LamboError, MatchStrategy};

/// Scoring weights for daemon composite (spec §9): recency / frequency / session_activity / density.
///
/// Every field is a public `f64` and this struct deserializes from `lambo.toml`
/// / JSON, so `NaN`, `±inf` and negatives are all *admissible inputs*. Spec
/// §5.7 requires finite composites and GC compares the composite against a
/// threshold, where a `NaN` weight would silently disable collection
/// (`NaN < x == false`). Weights are therefore **sanitized at the point of
/// use** — [`ScoringWeights::sanitized`], applied by
/// [`crate::daemon::score::score`] — rather than rejected at parse time: a
/// mis-typed weight degrades that one dimension to zero instead of failing the
/// session.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoringWeights {
    pub recency: f64,
    pub frequency: f64,
    pub session_activity: f64,
    pub density: f64,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            recency: 0.25,
            frequency: 0.20,
            session_activity: 0.20,
            density: 0.35,
        }
    }
}

impl ScoringWeights {
    /// Every non-finite or negative weight replaced by `0.0` (ALGO-10).
    ///
    /// A zeroed weight drops its dimension out of the composite; it can never
    /// poison the whole score. Idempotent, and the identity on any valid set.
    pub fn sanitized(self) -> Self {
        Self {
            recency: sane_weight(self.recency),
            frequency: sane_weight(self.frequency),
            session_activity: sane_weight(self.session_activity),
            density: sane_weight(self.density),
        }
    }

    /// True when every weight is finite and non-negative (i.e. `sanitized` is
    /// the identity). Callers that prefer to fail loudly check this first.
    pub fn is_valid(self) -> bool {
        self == self.sanitized()
    }
}

/// A weight usable in the composite: finite and non-negative, else `0.0`.
fn sane_weight(w: f64) -> f64 {
    if w.is_finite() && w >= 0.0 {
        w
    } else {
        0.0
    }
}

/// Final recall mix: `daemon_score * w_daemon + query_relevance * w_query`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallWeights {
    pub w_daemon: f64,
    pub w_query: f64,
}

impl Default for RecallWeights {
    fn default() -> Self {
        Self {
            w_daemon: 0.5,
            w_query: 0.5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub backend_flush_interval: Duration,
    pub backend_flush_max_batch: usize,
    pub backend_flush_retries: u32,
    pub backend_log_max: usize,

    pub scoring: ScoringWeights,
    pub recall_weights: RecallWeights,

    /// Daemon poll interval — how often the loop rescores and re-runs the
    /// detectors (XP-7). This is the one daemon parameter nothing else can
    /// derive: it governs stale-detection latency, GC latency, and how long a
    /// hot-list entry can lag the graph. Default
    /// [`crate::daemon::DAEMON_TICK_INTERVAL`].
    pub daemon_tick_interval: Duration,

    pub hot_list_max: usize,
    pub conflict_recency_window: Duration,
    /// `drift_threshold` in hops (spec §9). `usize` — the metric is a hop
    /// count, which every consumer (`drift::detect`, `CycleParams`) indexes
    /// with; the previous `u32` forced a cast at every use site (XP-7).
    pub drift_threshold: usize,

    pub gc_interval: u64,
    pub max_canonical_nodes: usize,

    pub canonization_min_peer_count: usize,
    pub canonization_edge_min_age: Duration,
    pub canonization_eval_interval: Duration,
    pub canonization_eval_batch_size: usize,
    pub canonization_repromotion_cooldown: Duration,

    pub semantic_match_threshold: f64,
    pub max_cooccurrence_per_derive: usize,

    pub default_top_k: usize,
    pub default_max_tokens: usize,
    pub default_traversal_depth: usize,

    pub match_strategy: MatchStrategy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend_flush_interval: Duration::from_secs(1),
            backend_flush_max_batch: 500,
            backend_flush_retries: 3,
            backend_log_max: 50_000,

            scoring: ScoringWeights::default(),
            recall_weights: RecallWeights::default(),

            daemon_tick_interval: crate::daemon::DAEMON_TICK_INTERVAL,

            hot_list_max: crate::daemon::hotlist::HOT_LIST_MAX,
            conflict_recency_window: crate::daemon::conflict::CONFLICT_RECENCY_WINDOW,
            drift_threshold: crate::daemon::drift::DRIFT_THRESHOLD,

            gc_interval: 10_000,
            max_canonical_nodes: 1000,

            canonization_min_peer_count: 20,
            canonization_edge_min_age: Duration::from_secs(60),
            canonization_eval_interval: Duration::from_secs(60),
            canonization_eval_batch_size: 50,
            canonization_repromotion_cooldown: Duration::from_secs(300),

            semantic_match_threshold: 0.85,
            max_cooccurrence_per_derive: 10,

            default_top_k: 5,
            default_max_tokens: 500,
            default_traversal_depth: 2,

            match_strategy: MatchStrategy::Hybrid,
        }
    }
}

impl Config {
    /// Validate product-level knobs that must fail closed rather than poison
    /// semantic selection. Callers that construct `Config` programmatically
    /// should invoke this before starting a session; the hybrid entry point
    /// repeats the threshold check at the trust boundary.
    pub fn validate(&self) -> Result<(), LamboError> {
        if !self.semantic_match_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.semantic_match_threshold)
        {
            return Err(LamboError::Config(format!(
                "semantic_match_threshold must be finite and in [0, 1], got {}",
                self.semantic_match_threshold
            )));
        }
        if self.default_top_k > crate::store::MAX_VECTOR_CANDIDATE_LIMIT {
            return Err(LamboError::Config(format!(
                "default_top_k {} exceeds maximum {}",
                self.default_top_k,
                crate::store::MAX_VECTOR_CANDIDATE_LIMIT
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Level B process file — store + embedder selection
// ---------------------------------------------------------------------------

/// Daemon cadence overrides (`[daemon]` in `lambo.toml`).
///
/// Everything here is a *cadence*, never a threshold. The bar a concept has to
/// clear to become Canonical lives in `canon::stage{1,2,3}` and is not settable
/// from a file: blast radius, distinct interactions, coverage and the peer
/// score cut are the product's judgement and stay that way.
///
/// This exists because the default cadence puts canonization out of reach of
/// any ordinary session. GC runs every `gc_interval` *mutations* (default
/// 10 000) and Stage 1 requires `gc_survived >= 3`, so a concept cannot be
/// promoted until the session has taken 30 000 mutations. `lambo demo` only
/// shows the state machine working because it sets `gc_interval` to 1
/// internally. Without a way to say the same thing from a config file, a real
/// deployment can run for weeks and never promote anything.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Mutations between GC sweeps. Lower makes `gc_survived` accumulate sooner.
    pub gc_interval: Option<u64>,
    /// Seconds between canonization evaluation passes.
    pub canonization_eval_interval_secs: Option<u64>,
}

impl DaemonConfig {
    /// Apply the set overrides onto a [`Config`], leaving unset keys alone.
    pub fn apply_to(&self, cfg: &mut Config) {
        if let Some(v) = self.gc_interval {
            cfg.gc_interval = v;
        }
        if let Some(secs) = self.canonization_eval_interval_secs {
            cfg.canonization_eval_interval = std::time::Duration::from_secs(secs);
        }
    }
}

/// On-disk process config (`lambo.toml`). Product knobs stay on [`Config`];
/// this file chooses which compiled adapters to run, and may override daemon
/// *cadence* (see [`DaemonConfig`]) but never a canonization threshold.
///
/// Unknown keys are rejected (`deny_unknown_fields`) so typos like `knd` / `[embeder]`
/// fail closed instead of silently using defaults.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LamboFile {
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub embedder: EmbedderConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl LamboFile {
    /// Parse TOML text.
    pub fn from_toml_str(s: &str) -> Result<Self, LamboError> {
        toml::from_str(s).map_err(|e| LamboError::Config(format!("lambo.toml: {e}")))
    }

    /// Load from a path.
    pub fn load_path(path: impl AsRef<Path>) -> Result<Self, LamboError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .map_err(|e| LamboError::Config(format!("read {}: {e}", path.display())))?;
        Self::from_toml_str(&text)
    }

    /// Resolve config path: `explicit` → `LAMBO_CONFIG` → `./lambo.toml` if present.
    pub fn discover_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.to_path_buf());
        }
        if let Ok(p) = std::env::var("LAMBO_CONFIG") {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        let local = PathBuf::from("lambo.toml");
        if local.is_file() {
            return Some(local);
        }
        None
    }

    /// Load file (if any), then overlay environment (env wins).
    ///
    /// Precedence: env > file > defaults (see Level B note).
    pub fn load_resolved(explicit: Option<&Path>) -> Result<Self, LamboError> {
        let mut file = if let Some(path) = Self::discover_path(explicit) {
            Self::load_path(path)?
        } else {
            Self::default()
        };
        file.store = file
            .store
            .overlay_env()
            .map_err(|e| LamboError::Config(e.to_string()))?;
        file.embedder = file
            .embedder
            .overlay_env()
            .map_err(|e: EmbedError| LamboError::Config(e.to_string()))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbedderKind;
    use crate::store::StoreKind;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.backend_flush_interval, Duration::from_secs(1));
        assert_eq!(c.backend_flush_max_batch, 500);
        assert_eq!(c.backend_flush_retries, 3);
        assert_eq!(c.backend_log_max, 50_000);

        assert_eq!(c.scoring.recency, 0.25);
        assert_eq!(c.scoring.frequency, 0.20);
        assert_eq!(c.scoring.session_activity, 0.20);
        assert_eq!(c.scoring.density, 0.35);

        // XP-7: the daemon tick has a named default, and the spec §9 knobs the
        // daemon also declares as consts must agree with them here — a drift
        // between the two is exactly what CycleParams' literal duplication hid.
        assert_eq!(c.daemon_tick_interval, Duration::from_secs(1));
        assert_eq!(c.hot_list_max, 1000);
        assert_eq!(c.conflict_recency_window, Duration::from_secs(30));
        assert_eq!(c.drift_threshold, 5);

        assert_eq!(c.gc_interval, 10_000);
        assert_eq!(c.max_canonical_nodes, 1000);

        assert_eq!(c.canonization_min_peer_count, 20);
        assert_eq!(c.canonization_edge_min_age, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_interval, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_batch_size, 50);
        assert_eq!(
            c.canonization_repromotion_cooldown,
            Duration::from_secs(300)
        );

        assert!((c.semantic_match_threshold - 0.85).abs() < 1e-12);
        assert_eq!(c.max_cooccurrence_per_derive, 10);

        assert_eq!(c.default_top_k, 5);
        assert_eq!(c.default_max_tokens, 500);
        assert_eq!(c.default_traversal_depth, 2);

        assert_eq!(c.match_strategy, MatchStrategy::Hybrid);
    }

    #[test]
    fn config_json_roundtrip() {
        let c = Config::default();
        // Duration serializes as {secs, nanos} with serde — use a dedicated
        // intermediate or skip full Config JSON; assert scoring round-trips.
        let s = serde_json::to_string(&c.scoring).unwrap();
        let back: ScoringWeights = serde_json::from_str(&s).unwrap();
        assert_eq!(c.scoring, back);
    }

    #[test]
    fn semantic_threshold_validation_fails_closed() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.01] {
            let c = Config {
                semantic_match_threshold: bad,
                ..Config::default()
            };
            assert!(c.validate().is_err(), "accepted invalid threshold {bad}");
        }
        for good in [0.0, 0.85, 1.0] {
            let c = Config {
                semantic_match_threshold: good,
                ..Config::default()
            };
            c.validate().unwrap();
        }
        let oversized = Config {
            default_top_k: crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
            ..Config::default()
        };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn lambo_file_example_parses() {
        let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lambo.example.toml"));
        // Strip comments is fine; toml crate accepts full file with comments.
        let f = LamboFile::from_toml_str(raw).unwrap();
        assert_eq!(f.store.kind, StoreKind::Memory);
        assert_eq!(f.embedder.kind, EmbedderKind::BgeM3);
        assert_eq!(f.embedder.dim, 1024);
        assert_eq!(
            f.embedder.llama_url.as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    #[test]
    fn lambo_file_empty_sections_default() {
        // Empty tables must not hard-fail; kind/dim use serde defaults.
        let f = LamboFile::from_toml_str("[store]\n[embedder]\n").unwrap();
        assert_eq!(f.store.kind, StoreKind::Memory);
        assert_eq!(f.embedder.kind, EmbedderKind::BgeM3);
        assert_eq!(f.embedder.dim, 1024);
    }

    #[test]
    fn lambo_file_rejects_empty_kind_strings() {
        assert!(LamboFile::from_toml_str("[store]\nkind = \"\"\n").is_err());
        assert!(LamboFile::from_toml_str("[embedder]\nkind = \"\"\n").is_err());
    }

    #[test]
    fn lambo_file_rejects_unknown_keys() {
        assert!(
            LamboFile::from_toml_str("[store]\nknd = \"cockroach\"\n").is_err(),
            "typo store field must fail closed"
        );
        assert!(
            LamboFile::from_toml_str("[embeder]\nkind = \"bge_m3\"\n").is_err(),
            "typo section must fail closed"
        );
        assert!(LamboFile::from_toml_str("extra = 1\n").is_err());
    }

    #[test]
    fn lambo_file_store_aliases() {
        let f = LamboFile::from_toml_str(
            r#"
[store]
kind = "mem"
[embedder]
kind = "fake"
"#,
        )
        .unwrap();
        assert_eq!(f.store.kind, StoreKind::Memory);
        assert_eq!(f.embedder.kind, EmbedderKind::Fixture);
        assert_eq!(f.embedder.dim, 1024);
    }

    #[test]
    fn discover_path_explicit_wins() {
        let p = PathBuf::from("/tmp/does-not-need-to-exist-lambo.toml");
        assert_eq!(LamboFile::discover_path(Some(p.as_path())), Some(p.clone()));
    }

    #[test]
    fn lambo_file_toml_roundtrip_kinds() {
        let f = LamboFile {
            store: StoreConfig {
                kind: StoreKind::Sqlite,
                dsn: None,
                path: Some("./x.db".into()),
            },
            embedder: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            daemon: Default::default(),
        };
        let s = toml::to_string(&f).unwrap();
        let back: LamboFile = toml::from_str(&s).unwrap();
        assert_eq!(f, back);
    }
}
