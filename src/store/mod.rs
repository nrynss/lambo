//! GraphStore trait, Level B factory, and optional adapters (P1 / P3).
//!
//! Packaging: Cargo features gate adapters (`store-memory`, `store-cockroach`,
//! `store-sqlite`); `lambo.toml` / env select among compiled kinds. See
//! `dev-diary/notes/level-b-pluggability.md`.

#[cfg(feature = "store-memory")]
mod memory;

#[cfg(feature = "store-memory")]
pub use memory::MemoryStore;

// T3.2 — CockroachDB durable adapter (spec §3.2/§3.3, §4). Feature: store-cockroach.
#[cfg(feature = "store-cockroach")]
pub mod cockroach;

// T3.5 — `load_session()` / startup materialization (see `load.rs`).
pub mod load;
// T3.4 — write-behind flush task (spec §2.4–§2.5); drains any GraphStore.
pub mod flush;

use async_trait::async_trait;
use bitflags::bitflags;
use serde::{Deserialize, Deserializer, Serialize};
use std::env;
use std::str::FromStr;
use std::time::Duration;

use crate::types::{
    CanonizationEvent, GraphSnapshot, InteractionSpan, MutationBatch, NodeId, Scored, SessionId,
    StoreError,
};

bitflags! {
    /// Adapter capabilities (spec §3.2).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        const VECTOR_SEARCH = 0b0001;
        const HISTORY = 0b0010;
    }
}

/// Durable / query surface — Lambo vocabulary only (spec §3.2).
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn init_schema(&self) -> Result<(), StoreError>;
    fn capabilities(&self) -> Capabilities;

    /// Fixed dense-vector column width when this store persists embeddings.
    ///
    /// * `Some(n)` — e.g. Cockroach `VECTOR(n)`; embedder output must be exactly `n`.
    /// * `None` — no vector column / no constraint (MemoryStore, SQLite without vectors).
    ///
    /// Dim is **not** a global product constant: the store schema is the authority.
    /// Checked at process resolution (`crate::resolve::check_vector_compatibility`).
    fn vector_dimensions(&self) -> Option<usize> {
        None
    }

    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError>;
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError>;

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    /// Requires [`Capabilities::VECTOR_SEARCH`]; adapters without it return
    /// [`StoreError::Capability`].
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
    ) -> Result<u64, StoreError>;

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
    ) -> Result<InteractionSpan, StoreError>;

    async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError>;
}

/// Durable store selector (TOML `store.kind` / `LAMBO_STORE`).
///
/// Deserialize accepts the same aliases as [`FromStr`] (trimmed, case-insensitive):
/// `memory|mem|ram`, `cockroach|crdb|postgres|pg`, `sqlite|sqlite3`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// In-RAM adapter (tests, fixture-ok tracks). Feature: `store-memory`.
    #[default]
    Memory,
    /// CockroachDB primary (P3 / T3.2). Feature: `store-cockroach`.
    Cockroach,
    /// SQLite offline / test tier (P3 / T3.3). Feature: `store-sqlite`.
    Sqlite,
}

impl<'de> Deserialize<'de> for StoreKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: StoreError| serde::de::Error::custom(e.to_string()))
    }
}

impl StoreKind {
    pub const fn feature_name(self) -> &'static str {
        match self {
            Self::Memory => "store-memory",
            Self::Cockroach => "store-cockroach",
            Self::Sqlite => "store-sqlite",
        }
    }

    /// Whether this kind's Cargo feature is compiled into the current binary.
    ///
    /// Note: `true` does not mean the adapter is fully implemented (see [`Self::is_ready`]).
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Memory => cfg!(feature = "store-memory"),
            Self::Cockroach => cfg!(feature = "store-cockroach"),
            Self::Sqlite => cfg!(feature = "store-sqlite"),
        }
    }

    /// Whether [`build_store`] can return a working adapter (feature on **and** impl exists).
    pub const fn is_ready(self) -> bool {
        match self {
            Self::Memory => cfg!(feature = "store-memory"),
            // T3.2: CockroachStore landed.
            Self::Cockroach => cfg!(feature = "store-cockroach"),
            // T3.3 not implemented yet.
            Self::Sqlite => false,
        }
    }
}

// Registry design note (do not "simplify" away):
// * `is_compiled()` is a *message* pre-check so we can say "rebuild with --features X"
//   without naming uncompiled types.
// * The real gate is the `#[cfg(feature = "...")]` arm that constructs the adapter.
// * Both are required: pre-check alone cannot reference uncompiled types; cfg alone
//   yields a poorer error when the arm is missing.

impl FromStr for StoreKind {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(StoreError::Backend(
                "empty store kind (expected memory | cockroach | sqlite)".into(),
            ));
        }
        match t.to_ascii_lowercase().as_str() {
            "memory" | "mem" | "ram" => Ok(Self::Memory),
            "cockroach" | "crdb" | "postgres" | "pg" => Ok(Self::Cockroach),
            "sqlite" | "sqlite3" => Ok(Self::Sqlite),
            other => Err(StoreError::Backend(format!(
                "unknown store kind {other:?} (expected memory | cockroach | sqlite)"
            ))),
        }
    }
}

impl std::fmt::Display for StoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory => write!(f, "memory"),
            Self::Cockroach => write!(f, "cockroach"),
            Self::Sqlite => write!(f, "sqlite"),
        }
    }
}

/// Configuration for [`build_store`].
///
/// `Debug` redacts `dsn` (often carries a password). Serde still round-trips the real
/// value so configs can be written intentionally — never log `toml::to_string` of a live DSN.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    #[serde(default)]
    pub kind: StoreKind,
    /// Cockroach / Postgres DSN (`LAMBO_COCKROACH_DSN`, then `DATABASE_URL`).
    #[serde(default)]
    pub dsn: Option<String>,
    /// SQLite file path or `sqlite::memory:`.
    #[serde(default)]
    pub path: Option<String>,
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreConfig")
            .field("kind", &self.kind)
            .field(
                "dsn",
                &self.dsn.as_ref().map(|_| "***REDACTED***".to_string()),
            )
            .field("path", &self.path)
            .finish()
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            kind: StoreKind::Memory,
            dsn: None,
            path: None,
        }
    }
}

impl StoreConfig {
    /// Resolve a non-empty DSN from env only.
    ///
    /// Prefer non-empty `LAMBO_COCKROACH_DSN`; else non-empty `DATABASE_URL`.
    /// Empty strings are treated as **unset** (same as other env knobs).
    pub fn dsn_from_env() -> Option<String> {
        env::var("LAMBO_COCKROACH_DSN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| env::var("DATABASE_URL").ok().filter(|s| !s.is_empty()))
    }

    fn env_kind() -> Result<Option<StoreKind>, StoreError> {
        match env::var("LAMBO_STORE") {
            Ok(s) if !s.trim().is_empty() => Ok(Some(s.parse()?)),
            _ => Ok(None),
        }
    }

    /// Build from environment only. Equivalent to `Self::default().overlay_env()`.
    pub fn from_env() -> Result<Self, StoreError> {
        Self::default().overlay_env()
    }

    /// Merge env over a base (e.g. from `lambo.toml`).
    ///
    /// Non-empty env values win over file. Empty env values are treated as unset and leave
    /// the file value intact (including empty `LAMBO_COCKROACH_DSN=` placeholders in `.env`).
    pub fn overlay_env(mut self) -> Result<Self, StoreError> {
        if let Some(k) = Self::env_kind()? {
            self.kind = k;
        }
        if let Some(dsn) = Self::dsn_from_env() {
            self.dsn = Some(dsn);
        }
        if let Ok(v) = env::var("LAMBO_SQLITE_PATH") {
            if !v.is_empty() {
                self.path = Some(v);
            }
        }
        Ok(self)
    }
}

fn missing_feature(kind: StoreKind) -> StoreError {
    StoreError::Backend(format!(
        "store kind `{kind}` is not compiled into this binary; rebuild with \
         `--features {}` (see dev-diary/notes/level-b-pluggability.md)",
        kind.feature_name()
    ))
}

/// Level B store registry. Fail-closed when the kind's feature is off or the adapter
/// is not implemented yet.
///
/// Prefer [`crate::resolve::resolve_backends`] at process start so store×embedder
/// compatibility is checked once and the same instances are handed to the command.
pub fn build_store(cfg: StoreConfig) -> Result<Box<dyn GraphStore>, StoreError> {
    // Pre-check for a clear rebuild hint (see module comment above is_compiled).
    if !cfg.kind.is_compiled() {
        return Err(missing_feature(cfg.kind));
    }
    match cfg.kind {
        StoreKind::Memory => {
            // Real gate: type only exists under this feature.
            #[cfg(feature = "store-memory")]
            {
                Ok(Box::new(MemoryStore::new()))
            }
            #[cfg(not(feature = "store-memory"))]
            {
                Err(missing_feature(StoreKind::Memory))
            }
        }
        StoreKind::Cockroach => {
            // Real gate: type only exists under this feature. The pool is created lazily
            // (connect_lazy) so constructing the adapter never touches the network.
            #[cfg(feature = "store-cockroach")]
            {
                Ok(Box::new(cockroach::CockroachStore::new(cfg)?))
            }
            #[cfg(not(feature = "store-cockroach"))]
            {
                Err(missing_feature(StoreKind::Cockroach))
            }
        }
        StoreKind::Sqlite => {
            // T3.3: typically `vector_dimensions() -> None` unless a BLOB path is added.
            Err(StoreError::Backend(
                "store-sqlite is enabled (or selected) but SqliteStore is not implemented yet \
                 (T3.3); use kind=memory until P3 lands"
                    .into(),
            ))
        }
    }
}

/// Build a store from environment variables only.
pub fn store_from_env() -> Result<Box<dyn GraphStore>, StoreError> {
    build_store(StoreConfig::from_env()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::env_lock;

    #[test]
    fn parses_store_kind() {
        assert_eq!("memory".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!("mem".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!("ram".parse::<StoreKind>().unwrap(), StoreKind::Memory);
        assert_eq!(
            "cockroach".parse::<StoreKind>().unwrap(),
            StoreKind::Cockroach
        );
        assert_eq!("crdb".parse::<StoreKind>().unwrap(), StoreKind::Cockroach);
        assert_eq!("pg".parse::<StoreKind>().unwrap(), StoreKind::Cockroach);
        assert_eq!("sqlite".parse::<StoreKind>().unwrap(), StoreKind::Sqlite);
        assert_eq!("sqlite3".parse::<StoreKind>().unwrap(), StoreKind::Sqlite);
        assert_eq!(
            "  postgres  ".parse::<StoreKind>().unwrap(),
            StoreKind::Cockroach
        );
        assert!("oracle".parse::<StoreKind>().is_err());
        assert!("".parse::<StoreKind>().is_err());
        assert!("   ".parse::<StoreKind>().is_err());
    }

    #[test]
    fn toml_kind_aliases_match_from_str() {
        #[derive(Deserialize)]
        struct Wrap {
            kind: StoreKind,
        }
        let w: Wrap = toml::from_str(r#"kind = "mem""#).unwrap();
        assert_eq!(w.kind, StoreKind::Memory);
        let w: Wrap = toml::from_str(r#"kind = "crdb""#).unwrap();
        assert_eq!(w.kind, StoreKind::Cockroach);
        let w: Wrap = toml::from_str(r#"kind = "  pg  ""#).unwrap();
        assert_eq!(w.kind, StoreKind::Cockroach);
        let w: Wrap = toml::from_str(r#"kind = "sqlite3""#).unwrap();
        assert_eq!(w.kind, StoreKind::Sqlite);
    }

    #[test]
    fn partial_store_toml_defaults() {
        let cfg: StoreConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        assert!(cfg.dsn.is_none());

        // Empty table body still defaults kind.
        let cfg: StoreConfig = toml::from_str("path = \"./x.db\"").unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        assert_eq!(cfg.path.as_deref(), Some("./x.db"));
    }

    #[test]
    #[cfg(feature = "store-memory")]
    fn builds_memory_store() {
        let s = build_store(StoreConfig::default()).unwrap();
        assert_eq!(s.capabilities(), Capabilities::empty());
        assert!(StoreKind::Memory.is_ready());
    }

    #[test]
    fn cockroach_build_behavior() {
        // T3.2: with the feature compiled, build_store returns a working adapter
        // (constructed lazily — no connection at build time); without it, fail closed
        // with a rebuild hint and never fall back to memory.
        let cfg = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: Some("postgresql://localhost/lambo".into()),
            path: None,
        };
        if StoreKind::Cockroach.is_compiled() {
            let s = build_store(cfg).unwrap();
            assert!(s.capabilities().contains(Capabilities::VECTOR_SEARCH));
            assert_eq!(s.vector_dimensions(), Some(1024));
            assert!(StoreKind::Cockroach.is_ready());
        } else {
            let Err(err) = build_store(cfg) else {
                panic!("expected err — silent fallback forbidden");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("not compiled") && msg.contains("store-cockroach"),
                "{msg}"
            );
            assert!(!msg.to_ascii_lowercase().contains("memory store"));
            assert!(!StoreKind::Cockroach.is_ready());
        }
    }

    #[test]
    fn sqlite_fail_closed_message() {
        let r = build_store(StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: Some("sqlite::memory:".into()),
        });
        let Err(err) = r else {
            panic!("expected err — silent fallback forbidden");
        };
        let msg = err.to_string();
        if StoreKind::Sqlite.is_compiled() {
            assert!(
                msg.contains("T3.3") || msg.contains("not implemented"),
                "{msg}"
            );
        } else {
            assert!(
                msg.contains("not compiled") && msg.contains("store-sqlite"),
                "{msg}"
            );
        }
        assert!(!StoreKind::Sqlite.is_ready());
    }

    #[test]
    fn no_silent_fallback_to_memory() {
        // Selecting cockroach must not return Ok(MemoryStore).
        let r = build_store(StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: None,
            path: None,
        });
        assert!(r.is_err());
    }

    #[test]
    fn feature_names() {
        assert_eq!(StoreKind::Memory.feature_name(), "store-memory");
        assert_eq!(StoreKind::Cockroach.feature_name(), "store-cockroach");
        assert_eq!(StoreKind::Sqlite.feature_name(), "store-sqlite");
    }

    #[test]
    fn empty_toml_kind_rejected() {
        let r = toml::from_str::<StoreConfig>(r#"kind = """#);
        assert!(r.is_err(), "empty kind string must not parse");
    }

    #[test]
    fn dsn_from_env_and_overlay_aligned() {
        let _g = env_lock();
        // Clean slate.
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_SQLITE_PATH");

        let base = StoreConfig {
            kind: StoreKind::Memory,
            dsn: Some("toml-dsn".into()),
            path: None,
        };

        // No DSN env → keep TOML; from_env has no file → None.
        let o = base.clone().overlay_env().unwrap();
        assert_eq!(o.dsn.as_deref(), Some("toml-dsn"));
        assert_eq!(StoreConfig::from_env().unwrap().dsn, None);

        // Empty primary (placeholder in .env) does NOT wipe TOML; secondary still applies.
        env::set_var("LAMBO_COCKROACH_DSN", "");
        env::remove_var("DATABASE_URL");
        assert_eq!(StoreConfig::dsn_from_env(), None);
        let o = base.clone().overlay_env().unwrap();
        assert_eq!(
            o.dsn.as_deref(),
            Some("toml-dsn"),
            "empty primary env must leave file dsn intact"
        );

        // Empty primary, secondary set → both paths use secondary.
        env::set_var("DATABASE_URL", "from-database-url");
        assert_eq!(
            StoreConfig::dsn_from_env().as_deref(),
            Some("from-database-url")
        );
        let o = base.clone().overlay_env().unwrap();
        assert_eq!(o.dsn.as_deref(), Some("from-database-url"));
        assert_eq!(
            StoreConfig::from_env().unwrap().dsn.as_deref(),
            Some("from-database-url")
        );

        // Primary non-empty beats secondary.
        env::set_var("LAMBO_COCKROACH_DSN", "from-primary");
        assert_eq!(StoreConfig::dsn_from_env().as_deref(), Some("from-primary"));
        let o = base.clone().overlay_env().unwrap();
        assert_eq!(o.dsn.as_deref(), Some("from-primary"));
        assert_eq!(
            StoreConfig::from_env().unwrap().dsn.as_deref(),
            Some("from-primary")
        );

        // Both empty (vars present) → leave file value (empty == unset).
        env::set_var("LAMBO_COCKROACH_DSN", "");
        env::set_var("DATABASE_URL", "");
        assert_eq!(StoreConfig::dsn_from_env(), None);
        let o = base.overlay_env().unwrap();
        assert_eq!(o.dsn.as_deref(), Some("toml-dsn"));
        assert_eq!(StoreConfig::from_env().unwrap().dsn, None);

        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
    }

    #[test]
    fn dsn_redacted_in_debug() {
        let cfg = StoreConfig {
            kind: StoreKind::Cockroach,
            dsn: Some("postgresql://user:s3cret@host:26257/lambo".into()),
            path: None,
        };
        let s = format!("{cfg:?}");
        assert!(s.contains("REDACTED"), "{s}");
        assert!(!s.contains("s3cret"), "{s}");
    }

    #[test]
    fn from_env_is_default_overlay() {
        let _g = env_lock();
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        env::remove_var("LAMBO_SQLITE_PATH");
        assert_eq!(
            StoreConfig::from_env().unwrap(),
            StoreConfig::default().overlay_env().unwrap()
        );
        env::set_var("LAMBO_STORE", "sqlite");
        env::set_var("LAMBO_SQLITE_PATH", "/tmp/x.db");
        assert_eq!(
            StoreConfig::from_env().unwrap(),
            StoreConfig::default().overlay_env().unwrap()
        );
        env::remove_var("LAMBO_STORE");
        env::remove_var("LAMBO_SQLITE_PATH");
    }

    #[test]
    fn unknown_toml_field_rejected() {
        assert!(toml::from_str::<StoreConfig>(r#"knd = "cockroach""#).is_err());
    }

    #[test]
    fn empty_lambo_store_env_keeps_default_kind() {
        let _g = env_lock();
        env::set_var("LAMBO_STORE", "");
        env::remove_var("LAMBO_COCKROACH_DSN");
        env::remove_var("DATABASE_URL");
        let cfg = StoreConfig::from_env().unwrap();
        assert_eq!(cfg.kind, StoreKind::Memory);
        let o = StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: None,
        }
        .overlay_env()
        .unwrap();
        // Empty LAMBO_STORE is unset → keep file kind.
        assert_eq!(o.kind, StoreKind::Sqlite);
        env::remove_var("LAMBO_STORE");
    }
}
