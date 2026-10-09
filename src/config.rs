//! Runtime configuration — named defaults from the v0.1 hackathon spec,
//! plus Level B process file (`lambo.toml`) for store/embedder selection.
//!
//! See `dev-diary/notes/level-b-pluggability.md`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::canon::PromotionPolicy;
use crate::embed::{EmbedError, EmbedderConfig};
use crate::store::StoreConfig;
use crate::types::{LamboError, MatchStrategy};

mod serve;
/// An environment variable name for an error message, or `(value not shown)`
/// when it may be a pasted secret (shared by `[serve]` and `[recall]`).
pub(crate) use serve::shown_env;
pub use serve::{
    CredentialConfig, InlineToken, ProjectConfig, ServeConfig, ServeCredential,
    DEFAULT_ATTACH_CONCURRENCY, DEFAULT_IDLE_DETACH_SECS, DEFAULT_MAX_ATTACHED,
    EVERY_HOSTED_SESSION, RESERVED_CREDENTIAL_NAMES, SERVE_UNENFORCED_NOTICE,
};

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
    /// Wall-clock bound on the time between GC sweeps (issue #29): a session
    /// that has not taken `gc_interval` mutations still sweeps once this much
    /// time has passed since its last sweep, provided it took at least
    /// [`Config::gc_idle_floor`] mutations since then. Default 24h. A cadence,
    /// not a threshold — see [`DaemonConfig`].
    pub gc_max_interval: Duration,
    /// Minimum session mutations since the last sweep before the
    /// `gc_max_interval` bound may trigger one (issue #29). Default 100. An idle
    /// session never sweeps on time alone, or `gc_survived` would measure age
    /// rather than surviving eviction pressure. Does not gate the
    /// `gc_interval` trigger.
    pub gc_idle_floor: u64,
    pub max_canonical_nodes: usize,

    pub canonization_min_peer_count: usize,
    pub canonization_edge_min_age: Duration,
    pub canonization_eval_interval: Duration,
    pub canonization_eval_batch_size: usize,
    pub canonization_repromotion_cooldown: Duration,

    /// Which promotion policy canonization's Stage 1 runs (C1).
    ///
    /// A *selector*, not a threshold — it chooses which predicate reads the
    /// canonization knobs above, and restates none of them. The bars
    /// themselves still live in `canon::stage{1,2,3}` and are still not
    /// settable from a file (see [`DaemonConfig`]).
    ///
    /// Unlike [`MatchStrategy`], this field's `Default` and the product
    /// default agree: both are `Swarm`.
    pub promotion_policy: PromotionPolicy,

    pub semantic_match_threshold: f64,
    pub max_cooccurrence_per_derive: usize,

    pub default_top_k: usize,
    pub default_max_tokens: usize,
    pub default_traversal_depth: usize,

    /// Recall's matching rule, the write path's embedding rule, **and** the
    /// call-time validation rule set — see [`MatchStrategy`], which documents
    /// all three and which of them carries the availability consequence. The
    /// product default is `Hybrid` (below), which is **not**
    /// `MatchStrategy`'s own `Default`.
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
            gc_max_interval: crate::daemon::gc::GC_MAX_INTERVAL,
            gc_idle_floor: crate::daemon::gc::GC_IDLE_FLOOR,
            max_canonical_nodes: 1000,

            canonization_min_peer_count: 20,
            canonization_edge_min_age: Duration::from_secs(60),
            canonization_eval_interval: Duration::from_secs(60),
            canonization_eval_batch_size: 50,
            canonization_repromotion_cooldown: Duration::from_secs(300),

            promotion_policy: PromotionPolicy::Swarm,

            semantic_match_threshold: crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT,
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
        // The three `Duration` cadences feed `tokio::interval`, which panics on
        // a zero period; `gc_interval` is a mutation counter where 0 means GC
        // every cycle — both must fail closed.
        if self.gc_interval == 0 {
            return Err(LamboError::Config(format!(
                "gc_interval must be >= 1 (a mutation counter), got {}",
                self.gc_interval
            )));
        }
        // Issue #29: a zero time bound would sweep on every cycle once the
        // floor is met, and a zero floor would sweep an idle session on time
        // alone — the exact age-not-pressure `gc_survived` the floor exists to
        // prevent. Both fail closed.
        if self.gc_max_interval == Duration::ZERO {
            return Err(LamboError::Config(format!(
                "gc_max_interval must be > 0, got {:?}",
                self.gc_max_interval
            )));
        }
        if self.gc_idle_floor == 0 {
            return Err(LamboError::Config(format!(
                "gc_idle_floor must be >= 1 (session mutations since the last sweep), got {}",
                self.gc_idle_floor
            )));
        }
        if self.daemon_tick_interval == Duration::ZERO {
            return Err(LamboError::Config(format!(
                "daemon_tick_interval must be > 0, got {:?}",
                self.daemon_tick_interval
            )));
        }
        if self.backend_flush_interval == Duration::ZERO {
            return Err(LamboError::Config(format!(
                "backend_flush_interval must be > 0, got {:?}",
                self.backend_flush_interval
            )));
        }
        if self.canonization_eval_interval == Duration::ZERO {
            return Err(LamboError::Config(format!(
                "canonization_eval_interval must be > 0, got {:?}",
                self.canonization_eval_interval
            )));
        }
        // C1's Solo refusal lived here while the solo scorer was
        // unimplemented ("a policy that cannot promote is indistinguishable
        // from one that merely promoted nothing — fail where the reason can be
        // named"). C2 landed the formula, so both the refusal and its
        // `unimplemented!()` backstop are gone together: every variant of
        // `promotion_policy` names a policy that can run a cycle, and there is
        // nothing left to refuse here.
        Ok(())
    }

    /// Advisory problems with a valid config: settings that are accepted but
    /// cannot do what they say. The resolve path logs each at WARN
    /// (`resolve::resolve_backends`); nothing is refused.
    ///
    /// * `gc_idle_floor >= gc_interval` (issue #29): the time trigger needs at
    ///   least `gc_idle_floor` mutations since the last sweep, and the
    ///   mutation trigger fires at `gc_interval` of them, so the mutation
    ///   trigger always wins and `gc_max_interval` never fires. Equality is
    ///   included: at exactly `gc_interval` mutations the mutation trigger is
    ///   checked first. Not an error, because sweeping on mutations alone is a
    ///   legitimate (pre-#29) configuration — only the time bound is dead.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.gc_idle_floor >= self.gc_interval {
            out.push(format!(
                "gc_idle_floor ({}) >= gc_interval ({}): the gc_max_interval time trigger can \
                 never fire (the mutation trigger always reaches its count first); lower \
                 [daemon] gc_idle_floor below gc_interval to enable timed sweeps",
                self.gc_idle_floor, self.gc_interval
            ));
        }
        out
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
/// any ordinary session **under the default `Swarm` policy**. GC runs every
/// `gc_interval` *mutations* (default 10 000) and swarm's Stage 1 requires
/// `gc_survived >= 3`, so a concept cannot be promoted until the session has
/// taken 30 000 mutations. `lambo demo` only shows the state machine working
/// because it sets `gc_interval` to 1 internally. Without a way to say the same
/// thing from a config file, a real deployment can run for weeks and never
/// promote anything.
///
/// # The 30 000 count is deployment-lifetime (issue #17)
///
/// "The session has taken 30 000 mutations" is measured over the
/// **deployment's** lifetime, not the writer process's: the mutation epoch
/// persists with the session (`sessions.mutation_epoch`; the flush stamps it,
/// the startup load resumes it), so writer restarts no longer reset the
/// counter toward zero. Before that, a low-write single-writer deployment
/// could never cross even one sweep in any single process — GC never ran,
/// `gc_survived` never left 0, and Stage 1 was closed by construction. The
/// cadence itself is unchanged: every threshold (peer count, `gc_survived >=
/// 3`, the P90 cut) stays exactly as designed.
///
/// # Both paragraphs above are policy-conditional (C2)
///
/// `promotion_policy = "Solo"` reads none of it. [`crate::canon::SoloScorer`]
/// scores on recurrence — `gc_survived`, blast radius, distinct interactions
/// and coverage are all ignored, and the score's own bands drive the ladder in
/// place of the store-evidence stages — so a lone writer reaches Canonical in a
/// handful of mutations with zero GC sweeps. **With `Solo` you do not need to
/// lower `gc_interval`**; lowering it changes nothing about promotion. The
/// "30 000 mutations" arithmetic and the "not settable from a file" framing
/// both describe swarm, which remains the default.
///
/// # GC also sweeps on time (issue #29)
///
/// At human pace even the deployment-lifetime count takes months: the Metal
/// dogfood rig took ~3.3k writes in 13 days. GC therefore also sweeps when
/// `gc_max_interval_secs` (default 86 400, one day) has elapsed since the
/// session's last sweep **and** at least `gc_idle_floor` (default 100) session
/// mutations happened since then — whichever of the two triggers comes first.
/// The time of the last sweep persists with the session (`sessions.last_gc_at`,
/// beside `last_gc_epoch`), so a restart does not reset the clock and does not
/// by itself cause a sweep: a restarted writer sweeps only when one was already
/// due by the stored mark (down past the interval with at least the floor of
/// unswept mutations — then once on its first cycle, not N times for N days).
/// A never-swept session anchors the clock on first attach instead. Both
/// keys are cadences; neither changes what a sweep collects or any promotion
/// bar. Like the other `[daemon]` keys they have no environment overlay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Mutations between GC sweeps. Lower makes `gc_survived` accumulate sooner.
    pub gc_interval: Option<u64>,
    /// Seconds between canonization evaluation passes.
    pub canonization_eval_interval_secs: Option<u64>,
    /// Maximum seconds between GC sweeps of a session that is still taking
    /// writes (issue #29); see the type docs. Named `_secs` like
    /// `canonization_eval_interval_secs`, the other duration in this table.
    #[serde(default)]
    pub gc_max_interval_secs: Option<u64>,
    /// Session mutations since the last sweep below which the time bound does
    /// not fire (issue #29). Does not gate `gc_interval`.
    #[serde(default)]
    pub gc_idle_floor: Option<u64>,
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
        if let Some(secs) = self.gc_max_interval_secs {
            cfg.gc_max_interval = std::time::Duration::from_secs(secs);
        }
        if let Some(v) = self.gc_idle_floor {
            cfg.gc_idle_floor = v;
        }
    }
}

/// On-disk process config (`lambo.toml`). This file chooses which compiled
/// adapters to run, may override daemon *cadence* (see [`DaemonConfig`]), and
/// selects the canonization promotion policy, but never a canonization
/// threshold.
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
    /// Canonization's Stage-1 promotion selector. `None` deliberately means
    /// "leave the product default alone", rather than making the process file
    /// carry a second default that could drift from [`Config::default`].
    ///
    /// Parsed by `PromotionPolicy::from_str` rather than serde's
    /// exact-`PascalCase` derive, so the file accepts what
    /// `LAMBO_PROMOTION_POLICY` accepts — trimmed and case-insensitive, like
    /// `store.kind` and `embedder.kind`. A file that refused `"solo"` while the
    /// environment took it would be a casing rule that exists nowhere else in
    /// this file.
    #[serde(
        default,
        deserialize_with = "crate::canon::deserialize_promotion_policy"
    )]
    pub promotion_policy: Option<PromotionPolicy>,
    /// `[recall]`: an optional recall tier beside the durable store (#18).
    /// `None` (no section) keeps the store exactly as `[store]` builds it. A
    /// section naming a tier this binary was not built with is refused at
    /// resolve, never ignored (see `store::recall_tier`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<crate::store::RecallConfig>,
    /// Multi-session serving (`[serve]`, #32). Parsed and validated only:
    /// nothing reads it at runtime until #32's later PRs, so an absent table
    /// and a present one serve exactly as before. Not serialized when empty,
    /// so a file written from a [`LamboFile`] without `[serve]` stays readable
    /// by a binary that predates it. See [`ServeConfig`].
    #[serde(default, skip_serializing_if = "ServeConfig::is_empty")]
    pub serve: ServeConfig,
}

/// A `lambo.toml` parse error, with its position and **without** the source
/// line.
///
/// `toml::de::Error`'s `Display` quotes the offending line, and the line can
/// hold a secret: a misspelled key beside a DSN with a password, or a token
/// under a typo of `token_env`. The message and the position are what an
/// operator needs to find the problem; the text is already in their file.
fn toml_error(src: &str, err: &toml::de::Error) -> LamboError {
    let message = redact_quoted_values(err.message().trim_end());
    let position = err.span().map(|span| {
        let before = src.get(..span.start).unwrap_or_default();
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        format!(" (line {line}, column {column})")
    });
    LamboError::Config(format!(
        "lambo.toml: {message}{}",
        position.unwrap_or_default()
    ))
}

/// serde's messages quote a wrong-typed or unknown value (`invalid type:
/// string "...", expected usize`, `unknown variant `...``), and that value can
/// be a secret pasted under the wrong key. Replace each such value with
/// `(value not shown)`; field names (`unknown field `dssn``) stay, since the
/// operator needs them and they are the file's own keys.
fn redact_quoted_values(message: &str) -> String {
    const HIDDEN: &str = "(value not shown)";
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    loop {
        let string_at = rest.find("string \"");
        let variant_at = rest.find("unknown variant `");
        let (at, prefix, close) = match (string_at, variant_at) {
            (Some(s), Some(v)) if v < s => (v, "unknown variant ", '`'),
            (Some(s), _) => (s, "string ", '"'),
            (None, Some(v)) => (v, "unknown variant ", '`'),
            (None, None) => break,
        };
        out.push_str(&rest[..at]);
        out.push_str(prefix);
        out.push_str(HIDDEN);
        // Skip the opening delimiter, then up to the matching close. A
        // string is rendered with `Debug`, so `\"` inside it is escaped.
        let body = &rest[at + prefix.len() + 1..];
        let mut escaped = false;
        let end = body.char_indices().find_map(|(i, c)| {
            let hit = c == close && !escaped;
            escaped = close == '"' && c == '\\' && !escaped;
            hit.then_some(i + c.len_utf8())
        });
        // An unterminated value: hide the remainder rather than guess.
        rest = end.map_or("", |e| &body[e..]);
    }
    out.push_str(rest);
    // Numbers are values too (`invalid type: integer `12345``), and a key is
    // shown only while it looks like a key: a quoted key such as
    // `"postgres://u:pw@h" = 1` reaches `unknown field` verbatim.
    let out = redact_backticked(&out, "integer `", |_| false);
    let out = redact_backticked(&out, "float `", |_| false);
    let out = redact_backticked(&out, "unknown field `", is_bare_key);
    redact_backticked(&out, "duplicate key `", is_bare_key)
}

/// A key short and plain enough to be a config key rather than a pasted value.
fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 40
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Replace the backtick-quoted text after each `prefix` (which ends with the
/// opening backtick) with `(value not shown)` unless `keep` accepts it.
fn redact_backticked(message: &str, prefix: &str, keep: fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(at) = rest.find(prefix) {
        let body = &rest[at + prefix.len()..];
        let (value, after) = match body.find('`') {
            Some(close) => (&body[..close], &body[close + 1..]),
            None => (body, ""),
        };
        out.push_str(&rest[..at + prefix.len()]);
        if keep(value) {
            out.push_str(value);
            out.push('`');
        } else {
            // Drop the opening backtick as well, matching the string form.
            out.pop();
            out.push_str("(value not shown)");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

impl LamboFile {
    /// Parse TOML text.
    ///
    /// Also runs [`ServeConfig::validate`], so a malformed `[serve]` table
    /// fails closed at the file boundary for every command, like an unknown
    /// key does.
    pub fn from_toml_str(s: &str) -> Result<Self, LamboError> {
        let file: Self = toml::from_str(s).map_err(|e| toml_error(s, &e))?;
        file.serve.validate()?;
        Ok(file)
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
        if let Ok(p) = std::env::var("LAMBO_CONFIG")
            && !p.is_empty()
        {
            return Some(PathBuf::from(p));
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
        // Non-empty env wins; an empty value is UNSET and leaves the file
        // value alone — the same rule `StoreConfig::overlay_env` and
        // `EmbedderConfig::overlay_env` apply to all nine of their variables,
        // and the rule the env table in `docs/reference/config.mdx` promises.
        // `var_os` hands back `Some("")` for an exported-but-empty variable, so
        // an empty `LAMBO_PROMOTION_POLICY=` placeholder in a `.env` (or in
        // this repo's own test harness) would otherwise be a hard startup
        // error rather than a no-op.
        if let Some(raw) = std::env::var_os("LAMBO_PROMOTION_POLICY") {
            let raw = raw.to_string_lossy();
            if !raw.trim().is_empty() {
                file.promotion_policy = Some(raw.parse::<PromotionPolicy>().map_err(|error| {
                    LamboError::Config(format!("LAMBO_PROMOTION_POLICY: {error}"))
                })?);
            }
        }
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
        assert_eq!(c.gc_max_interval, Duration::from_secs(24 * 60 * 60));
        assert_eq!(c.gc_idle_floor, 100);
        assert_eq!(c.max_canonical_nodes, 1000);

        assert_eq!(c.canonization_min_peer_count, 20);
        assert_eq!(c.canonization_edge_min_age, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_interval, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_batch_size, 50);
        assert_eq!(
            c.canonization_repromotion_cooldown,
            Duration::from_secs(300)
        );
        // C1: swarm is the shipped default and the whole point of the seam is
        // that it stays one. Since C2 nothing else defends that — `validate`
        // accepts either policy — so this assertion is the only thing standing
        // between an unset `promotion_policy` and moved behaviour.
        assert_eq!(c.promotion_policy, PromotionPolicy::Swarm);

        assert_eq!(
            c.semantic_match_threshold,
            crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT
        );
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

    /// C2 clean cutover: with the solo formula landed, `Solo` validates — the
    /// C1-era refusal (and its `unimplemented!()` backstop) is gone, and every
    /// `promotion_policy` value names a runnable cycle.
    ///
    /// Mutation: reintroduce a refusal arm for non-default policies in
    /// `Config::validate` → red on the `is_ok` assertion.
    #[test]
    fn every_promotion_policy_validates() {
        for policy in [PromotionPolicy::Swarm, PromotionPolicy::Solo] {
            let c = Config {
                promotion_policy: policy,
                ..Config::default()
            };
            c.validate()
                .unwrap_or_else(|e| panic!("{policy:?} must validate: {e}"));
        }
    }

    /// The refusal above must not be over-broad: the default config still
    /// validates. Without this, "reject every policy" would pass the test
    /// above and silently break every session.
    ///
    /// Mutation: make `validate` refuse unconditionally → red here.
    #[test]
    fn the_default_promotion_policy_validates() {
        assert_eq!(Config::default().promotion_policy, PromotionPolicy::Swarm);
        Config::default()
            .validate()
            .expect("the shipped default must validate");
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
    fn cadence_validation_fails_closed() {
        // Defaults must still validate Ok — the happy path stays intact.
        Config::default().validate().unwrap();

        let zero_gc = Config {
            gc_interval: 0,
            ..Config::default()
        };
        assert!(zero_gc.validate().is_err(), "gc_interval == 0 must fail");

        let zero_tick = Config {
            daemon_tick_interval: Duration::ZERO,
            ..Config::default()
        };
        assert!(
            zero_tick.validate().is_err(),
            "daemon_tick_interval == 0 must fail"
        );

        let zero_flush = Config {
            backend_flush_interval: Duration::ZERO,
            ..Config::default()
        };
        assert!(
            zero_flush.validate().is_err(),
            "backend_flush_interval == 0 must fail"
        );

        let zero_canon = Config {
            canonization_eval_interval: Duration::ZERO,
            ..Config::default()
        };
        assert!(
            zero_canon.validate().is_err(),
            "canonization_eval_interval == 0 must fail"
        );
    }

    #[test]
    fn daemon_config_zero_cadence_override_rejected() {
        // A `[daemon]` section turning a cadence to zero must fail validate()
        // rather than reach tokio::interval (which panics on `Duration::ZERO`).
        let mut cfg = Config::default();
        DaemonConfig {
            gc_interval: Some(0),
            canonization_eval_interval_secs: Some(0),
            ..Default::default()
        }
        .apply_to(&mut cfg);
        assert!(cfg.validate().is_err(), "zero overrides must fail");

        // Each override alone is also rejected.
        let mut only_canon = Config::default();
        DaemonConfig {
            gc_interval: None,
            canonization_eval_interval_secs: Some(0),
            ..Default::default()
        }
        .apply_to(&mut only_canon);
        assert!(
            only_canon.validate().is_err(),
            "zero canonization_eval_interval_secs override must fail"
        );
        // gc alone.
        let mut only_gc = Config::default();
        DaemonConfig {
            gc_interval: Some(0),
            canonization_eval_interval_secs: None,
            ..Default::default()
        }
        .apply_to(&mut only_gc);
        assert!(
            only_gc.validate().is_err(),
            "zero gc_interval override must fail"
        );

        // Sanity: a non-zero cadence override still validates.
        let mut ok = Config::default();
        DaemonConfig {
            gc_interval: Some(1),
            canonization_eval_interval_secs: Some(60),
            ..Default::default()
        }
        .apply_to(&mut ok);
        ok.validate().unwrap();
    }

    #[test]
    fn lambo_file_example_parses() {
        let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lambo.example.toml"));
        // Strip comments is fine; toml crate accepts full file with comments.
        let f = LamboFile::from_toml_str(raw).unwrap();
        assert_eq!(f.store.kind, StoreKind::Memory);
        assert_eq!(f.embedder.kind, EmbedderKind::BgeM3);
        assert_eq!(f.embedder.dim, 1024);
        assert_eq!(f.promotion_policy, None);
        // Issue #13: the example documents keep_warm_secs commented out, so
        // the shipped example resolves keep-warm to auto.
        assert_eq!(f.embedder.keep_warm_secs, None);
        assert_eq!(
            f.embedder.llama_url.as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }

    /// #18: the example's commented `[recall]` block is a working section
    /// once uncommented, and leaving it commented keeps the tier off.
    #[test]
    fn lambo_file_example_recall_block_parses_when_uncommented() {
        let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lambo.example.toml"));
        assert_eq!(LamboFile::from_toml_str(raw).unwrap().recall, None);
        let start = raw
            .find("# [recall]")
            .expect("the example documents [recall]");
        let block: String = raw[start..]
            .lines()
            .map(|l| l.strip_prefix("# ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        let f = LamboFile::from_toml_str(&block).unwrap();
        let recall = f.recall.expect("uncommented block selects the tier");
        assert_eq!(recall.kind, crate::store::RecallKind::Elastic);
        assert_eq!(recall.api_key.unwrap().env, "LAMBO_ES_API_KEY");
        assert_eq!(recall.index_prefix, "lambo");
        assert_eq!(recall.timeout_ms, Some(5000));
    }

    #[test]
    fn lambo_file_empty_sections_default() {
        // Empty tables must not hard-fail; kind/dim use serde defaults.
        let f = LamboFile::from_toml_str("[store]\n[embedder]\n").unwrap();
        assert_eq!(f.store.kind, StoreKind::Memory);
        assert_eq!(f.embedder.kind, EmbedderKind::BgeM3);
        assert_eq!(f.embedder.dim, 1024);
    }

    /// Issue #29 item 8: a floor at or above `gc_interval` makes the time
    /// trigger unreachable — warned about, not refused; the defaults and a
    /// floor below the interval are silent.
    #[test]
    fn an_idle_floor_at_or_above_gc_interval_warns_that_timed_sweeps_are_dead() {
        assert!(Config::default().warnings().is_empty());
        for (floor, interval, warns) in [(100, 101, false), (100, 100, true), (500, 100, true)] {
            let c = Config {
                gc_idle_floor: floor,
                gc_interval: interval,
                ..Config::default()
            };
            c.validate().expect("still a valid config");
            let w = c.warnings();
            assert_eq!(
                !w.is_empty(),
                warns,
                "floor {floor} interval {interval}: {w:?}"
            );
            if warns {
                assert!(w[0].contains("gc_max_interval") && w[0].contains("never fire"));
            }
        }
        // Through the file, the way `resolve_backends` builds it.
        let file =
            LamboFile::from_toml_str("[daemon]\ngc_interval = 50\ngc_idle_floor = 80\n").unwrap();
        let mut cfg = Config::default();
        file.daemon.apply_to(&mut cfg);
        assert_eq!(cfg.warnings().len(), 1);
    }

    /// Wire-visible `[daemon]` field names, parsed from real TOML text, must
    /// reach `validate()` and reject a zero cadence — pinning the file boundary
    /// that `resolve_backends` now fails closed on.
    #[test]
    fn lambo_file_zero_daemon_cadences_fail_validate() {
        // Both cadences zero in the file.
        let both = LamboFile::from_toml_str(
            "[daemon]\ngc_interval = 0\ncanonization_eval_interval_secs = 0\n\n[store]\n[embedder]\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        both.daemon.apply_to(&mut cfg);
        assert!(
            cfg.validate().is_err(),
            "both zero [daemon] cadences must fail"
        );

        // Each override alone: gc only.
        let gc_only =
            LamboFile::from_toml_str("[daemon]\ngc_interval = 0\n\n[store]\n[embedder]\n").unwrap();
        let mut cfg_gc = Config::default();
        gc_only.daemon.apply_to(&mut cfg_gc);
        assert!(
            cfg_gc.validate().is_err(),
            "zero gc_interval alone must fail"
        );

        // Each override alone: canonization eval only.
        let canon_only = LamboFile::from_toml_str(
            "[daemon]\ncanonization_eval_interval_secs = 0\n\n[store]\n[embedder]\n",
        )
        .unwrap();
        let mut cfg_canon = Config::default();
        canon_only.daemon.apply_to(&mut cfg_canon);
        assert!(
            cfg_canon.validate().is_err(),
            "zero canonization_eval_interval_secs alone must fail"
        );
    }

    /// Issue #29: the two time-bound keys parse from `[daemon]`, reach
    /// `Config`, leave unset keys alone, fail `validate()` at zero, and a typo
    /// is still a hard error.
    #[test]
    fn lambo_file_gc_time_bound_keys_parse_apply_and_validate() {
        let f = LamboFile::from_toml_str(
            "[daemon]\ngc_max_interval_secs = 3600\ngc_idle_floor = 25\n\n[store]\n[embedder]\n",
        )
        .unwrap();
        assert_eq!(f.daemon.gc_max_interval_secs, Some(3600));
        assert_eq!(f.daemon.gc_idle_floor, Some(25));
        let mut cfg = Config::default();
        f.daemon.apply_to(&mut cfg);
        assert_eq!(cfg.gc_max_interval, Duration::from_secs(3600));
        assert_eq!(cfg.gc_idle_floor, 25);
        assert_eq!(cfg.gc_interval, 10_000, "unset keys keep their default");
        cfg.validate().unwrap();

        // Absent keys leave the defaults.
        let mut untouched = Config::default();
        LamboFile::from_toml_str("[daemon]\ngc_interval = 50\n")
            .unwrap()
            .daemon
            .apply_to(&mut untouched);
        assert_eq!(untouched.gc_max_interval, Config::default().gc_max_interval);
        assert_eq!(untouched.gc_idle_floor, Config::default().gc_idle_floor);

        for zero in ["gc_max_interval_secs = 0", "gc_idle_floor = 0"] {
            let mut cfg = Config::default();
            LamboFile::from_toml_str(&format!("[daemon]\n{zero}\n"))
                .unwrap()
                .daemon
                .apply_to(&mut cfg);
            assert!(cfg.validate().is_err(), "{zero} must fail validate()");
        }

        for typo in [
            "gc_max_interval = 3600",
            "gc_max_interval_sec = 3600",
            "gc_idle_flor = 5",
        ] {
            assert!(
                LamboFile::from_toml_str(&format!("[daemon]\n{typo}\n")).is_err(),
                "unknown [daemon] key {typo:?} must fail closed"
            );
        }
    }

    #[test]
    fn lambo_file_rejects_empty_kind_strings() {
        assert!(LamboFile::from_toml_str("[store]\nkind = \"\"\n").is_err());
        assert!(LamboFile::from_toml_str("[embedder]\nkind = \"\"\n").is_err());
    }

    /// A `lambo.toml` parse error must not quote the offending source line.
    ///
    /// The `toml` crate's `Display` renders the line under the error, so a
    /// misspelled key next to a secret (a DSN with a password in it, or a
    /// bearer token under a typo of `token_env`) printed the secret into the
    /// startup error, and from there into a launchd or systemd log. The
    /// refusal keeps the parser's message and a line and column, and drops the
    /// source text.
    ///
    /// Mutation: format the error with `{e}` again → red.
    #[test]
    fn a_parse_error_names_the_line_but_never_quotes_it() {
        for (toml, needles) in [
            (
                "[store]\nkind = \"cockroach\"\ndssn = \"postgresql://u:fake-xyzzy@h/db\"\n",
                &["unknown field `dssn`", "line 3"][..],
            ),
            (
                "[[serve.credential]]\nname = \"agents\"\ntokn = \"fake-xyzzy\"\n",
                &["unknown field `tokn`", "line 3"][..],
            ),
            (
                "[store]\npath = \"fake-xyzzy\nkind = \"memory\"\n",
                &["line 2"][..],
            ),
        ] {
            let err = LamboFile::from_toml_str(toml).unwrap_err().to_string();
            assert!(!err.contains("xyzzy"), "the source line leaked: {err}");
            assert!(err.contains("lambo.toml: "), "{err}");
            for needle in needles {
                assert!(err.contains(needle), "{toml:?} must name {needle:?}: {err}");
            }
        }
    }

    /// A value under an enum-typed or number-typed key is never quoted back
    /// (#32 PR 1 review L1). A DSN or token pasted under `[store] kind`,
    /// `[embedder] kind`, `promotion_policy` or a numeric key reached the
    /// startup error through the kind parsers' "unknown ... kind {value}" and
    /// serde's `invalid type: string "..."`. The refusal lists what is
    /// accepted instead.
    ///
    /// Mutation: echo `{other:?}` in `StoreKind::from_str` again → red.
    #[test]
    fn a_value_under_a_typed_key_is_never_quoted() {
        let dsn = "postgresql://u:fake-xyzzy@h/db";
        for (toml, needle) in [
            (format!("[store]\nkind = \"{dsn}\"\n"), "sqlite"),
            (format!("[store]\nkind = \"  {dsn}  \"\n"), "memory"),
            (format!("[embedder]\nkind = \"{dsn}\"\n"), "fixture"),
            (format!("promotion_policy = \"{dsn}\"\n"), "Swarm"),
            (format!("[embedder]\ndim = \"{dsn}\"\n"), "invalid type"),
            (
                format!("[daemon]\ngc_interval = \"{dsn}\"\n"),
                "invalid type",
            ),
            (
                format!("[serve]\nmax_attached = \"{dsn}\"\n"),
                "invalid type",
            ),
        ] {
            let err = LamboFile::from_toml_str(&toml).unwrap_err().to_string();
            assert!(!err.contains("xyzzy"), "the value leaked: {err}");
            assert!(err.contains(needle), "{toml:?} must name {needle:?}: {err}");
            assert!(err.contains("line "), "{err}");
        }
        // The parsers themselves, which the environment overlay also calls.
        let store = dsn.parse::<StoreKind>().unwrap_err().to_string();
        assert!(
            !store.contains("xyzzy") && store.contains("postgres"),
            "{store}"
        );
        let embed = dsn.parse::<EmbedderKind>().unwrap_err().to_string();
        assert!(
            !embed.contains("xyzzy") && embed.contains("bge_m3"),
            "{embed}"
        );
        let policy = dsn.parse::<PromotionPolicy>().unwrap_err();
        assert!(
            !policy.contains("xyzzy") && policy.contains("Solo"),
            "{policy}"
        );
    }

    /// The redaction keeps field names and hides only values, including one
    /// whose `Debug` rendering holds an escaped quote.
    #[test]
    fn redact_quoted_values_hides_values_and_keeps_field_names() {
        assert_eq!(
            redact_quoted_values(r#"invalid type: string "a\"b-xyzzy", expected usize"#),
            "invalid type: string (value not shown), expected usize"
        );
        assert_eq!(
            redact_quoted_values("unknown variant `xyzzy`, expected one of `a`, `b`"),
            "unknown variant (value not shown), expected one of `a`, `b`"
        );
        assert_eq!(
            redact_quoted_values("unknown field `dssn`, expected `kind`"),
            "unknown field `dssn`, expected `kind`"
        );
        assert_eq!(
            redact_quoted_values(r#"string "xyzzy" then string "xyzzy" end"#),
            "string (value not shown) then string (value not shown) end"
        );
        assert_eq!(
            redact_quoted_values(r#"invalid value: string "unterminated-xyzzy"#),
            "invalid value: string (value not shown)"
        );
        assert_eq!(
            redact_quoted_values("invalid type: integer `48151623`, expected a string"),
            "invalid type: integer (value not shown), expected a string"
        );
        assert_eq!(
            redact_quoted_values("invalid type: float `4.815`, expected usize"),
            "invalid type: float (value not shown), expected usize"
        );
        assert_eq!(
            redact_quoted_values("unknown field `postgres://u:xyzzy@h/db`, expected `kind`"),
            "unknown field (value not shown), expected `kind`"
        );
        assert_eq!(
            redact_quoted_values("duplicate key `token-xyzzy.secret`"),
            "duplicate key (value not shown)"
        );
    }

    /// A DSN pasted as a quoted key never reaches the error; a plain typo
    /// still does, since the operator needs it.
    #[test]
    fn a_quoted_key_holding_a_secret_is_never_echoed() {
        let err = LamboFile::from_toml_str("[store]\n\"postgres://u:xyzzy@h/db\" = 1\n")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("xyzzy"), "{err}");
        assert!(err.contains("(value not shown)"), "{err}");
        let err = LamboFile::from_toml_str("[store]\nknd = \"memory\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("knd"), "{err}");
    }

    /// #18 under #32's redaction rules: a `[recall]` section is parsed by the
    /// same `from_toml_str`, so a URL with userinfo pasted under any key, or
    /// as the wrong type, never reaches the parse error.
    #[test]
    fn a_secret_looking_recall_url_never_reaches_a_parse_error() {
        const URL: &str = "https://elastic:xyzzy@es.example.com";
        let cases = [
            format!("[recall]\nkind = \"elastic\"\nurl = \"{URL}\"\ntimeout_ms = \"{URL}\"\n"),
            format!("[recall]\nkind = \"{URL}\"\nurl = \"{URL}\"\n"),
            format!("[recall]\nkind = \"elastic\"\nulr = \"{URL}\"\n"),
            format!("[recall]\nkind = \"elastic\"\n\"{URL}\" = 1\n"),
            format!("[recall]\nkind = \"elastic\"\nurl = \"{URL}\"\nurl = \"{URL}\"\n"),
            format!("[recall]\nkind = \"elastic\"\nurl = \"{URL}\"\napi_key = \"{URL}\"\n"),
            format!("[recall]\nkind = \"elastic\"\nurl = \"{URL}\"\nrefresh = \"{URL}\"\n"),
            format!("[recall]\nkind = \"elastic\"\nurl = \"{URL}\"\napi_key = {{ env = \"A\", x = \"{URL}\" }}\n"),
        ];
        for toml in &cases {
            let err = LamboFile::from_toml_str(toml).expect_err(toml).to_string();
            assert!(!err.contains("xyzzy"), "{toml}: {err}");
        }
        // A plain typo is still named, since the operator needs it.
        let err = LamboFile::from_toml_str(&cases[2]).unwrap_err().to_string();
        assert!(err.contains("ulr"), "{err}");
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

    /// The FILE surface goes through `PromotionPolicy::from_str`, not serde's
    /// exact-`PascalCase` derive (P3-a). `store.kind` and `embedder.kind` are
    /// both trimmed and case-insensitive with aliases, and every other value in
    /// this file is snake_case — a `promotion_policy` that took `Solo` in the
    /// environment and refused it in the file, or took `"Solo"` and refused
    /// `"solo"`, would be a casing rule with no sibling.
    #[test]
    fn lambo_file_promotion_policy_is_case_insensitive_and_defaults_to_none() {
        for spelling in ["Solo", "solo", "SOLO", "  Solo  ", "\tsOlO\n"] {
            let f = LamboFile::from_toml_str(&format!("promotion_policy = {spelling:?}\n"))
                .unwrap_or_else(|e| panic!("{spelling:?} must parse: {e}"));
            assert_eq!(
                f.promotion_policy,
                Some(PromotionPolicy::Solo),
                "{spelling:?}"
            );
        }
        for spelling in ["Swarm", "swarm", " SWARM "] {
            let f = LamboFile::from_toml_str(&format!("promotion_policy = {spelling:?}\n"))
                .unwrap_or_else(|e| panic!("{spelling:?} must parse: {e}"));
            assert_eq!(
                f.promotion_policy,
                Some(PromotionPolicy::Swarm),
                "{spelling:?}"
            );
        }
        // Absent stays absent: the file must not carry a second default that
        // could drift from `Config::default`.
        assert_eq!(LamboFile::default().promotion_policy, None);
        assert_eq!(
            LamboFile::from_toml_str("[store]\nkind = \"memory\"\n")
                .unwrap()
                .promotion_policy,
            None
        );
        // Still fails closed, and still names both halves — the leniency is
        // about casing, never about falling back to the default.
        for bogus in ["", "  ", "Solitary", "swarmy"] {
            let err = LamboFile::from_toml_str(&format!("promotion_policy = {bogus:?}\n"))
                .unwrap_err()
                .to_string();
            for needle in ["Swarm", "Solo"] {
                assert!(
                    err.contains(needle),
                    "{bogus:?} error must name {needle}: {err}"
                );
            }
        }
        assert!(
            LamboFile::from_toml_str("promotion_policy = \"Solitary\"\n")
                .unwrap_err()
                .to_string()
                .contains("Solitary"),
            "the refusal must quote the rejected value"
        );
    }

    /// P1-a: an exported-but-empty `LAMBO_PROMOTION_POLICY=` is **unset**.
    ///
    /// `var_os` returns `Some("")` for it, so parsing unconditionally turned an
    /// empty `.env` placeholder — and any harness that exports the variable
    /// blank — into a hard startup error, contradicting both the documented env
    /// rule and all nine sibling overrides.
    #[test]
    fn promotion_policy_empty_env_is_unset_and_leaves_the_file_value() {
        let env = crate::test_util::env_lock();
        let dir = scratch_config_dir("empty-env");
        let path = dir.join("lambo.toml");
        std::fs::write(&path, "promotion_policy = \"Solo\"\n").expect("config");

        for blank in ["", "   ", "\t"] {
            env.set("LAMBO_PROMOTION_POLICY", blank);
            let resolved = LamboFile::load_resolved(Some(&path))
                .unwrap_or_else(|e| panic!("blank {blank:?} must be unset, not an error: {e}"));
            assert_eq!(
                resolved.promotion_policy,
                Some(PromotionPolicy::Solo),
                "a blank override must leave the file value intact"
            );
        }

        // And with no file value at all, blank leaves the product default —
        // `None` here, which `resolve_backends` reads as "do not touch
        // `Config::default().promotion_policy`".
        std::fs::write(&path, "[store]\nkind = \"memory\"\n").expect("config");
        env.set("LAMBO_PROMOTION_POLICY", "");
        assert_eq!(
            LamboFile::load_resolved(Some(&path))
                .expect("blank env is unset")
                .promotion_policy,
            None
        );
    }

    /// A scratch directory for the env-override tests, unique per process and
    /// per call so two of them can never share a `lambo.toml`.
    fn scratch_config_dir(tag: &str) -> crate::test_util::ScratchDir {
        crate::test_util::ScratchDir::new(&format!("lambo-promotion-policy-{tag}"))
    }

    #[test]
    fn promotion_policy_env_beats_file_and_unknown_value_fails_closed() {
        let env = crate::test_util::env_lock();
        env.set("LAMBO_PROMOTION_POLICY", "Swarm");
        let dir = scratch_config_dir("env-wins");
        let path = dir.join("lambo.toml");
        std::fs::write(&path, "promotion_policy = \"Solo\"\n").expect("config");

        let resolved = LamboFile::load_resolved(Some(&path)).expect("env override");
        assert_eq!(resolved.promotion_policy, Some(PromotionPolicy::Swarm));

        // The env surface is the file surface's parser, so it is lenient in
        // exactly the same way and no more.
        env.set("LAMBO_PROMOTION_POLICY", " swarm ");
        assert_eq!(
            LamboFile::load_resolved(Some(&path))
                .expect("trimmed lowercase env override")
                .promotion_policy,
            Some(PromotionPolicy::Swarm)
        );

        env.set("LAMBO_PROMOTION_POLICY", "Everywhere");
        let err = LamboFile::load_resolved(Some(&path))
            .expect_err("unknown env value must fail at startup")
            .to_string();
        for needle in ["LAMBO_PROMOTION_POLICY", "Everywhere", "Swarm", "Solo"] {
            assert!(err.contains(needle), "error must name {needle}: {err}");
        }
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
                vector_dim: None,
            },
            embedder: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
                ..Default::default()
            },
            daemon: Default::default(),
            promotion_policy: Some(PromotionPolicy::Solo),
            recall: None,
            serve: Default::default(),
        };
        let s = toml::to_string(&f).unwrap();
        let back: LamboFile = toml::from_str(&s).unwrap();
        assert_eq!(f, back);
    }
}
