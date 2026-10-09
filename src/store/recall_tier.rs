//! The recall tier's configuration and registry arm (#18).
//!
//! A recall tier is **not** a store kind. The durable store (`[store]`) stays
//! the source of truth for everything that needs a transaction: fencing,
//! leases, canonization bookkeeping, graph queries. A recall tier sits beside
//! it and serves the vector leg of phase-1 recall from a search engine. The
//! adapter that composes the two is `TieredStore` (`store::tiered`, feature
//! `recall-elastic`).
//!
//! This module is compiled in every build, so a `[recall]` section is parsed
//! (and its unknown keys refused) whatever the binary was built with, and a
//! build without the tier's feature refuses the section by name instead of
//! ignoring it: Level B's fail-closed rule, the same one `store.kind` follows.
//!
//! ```toml
//! [recall]
//! kind = "elastic"
//! url = "https://my-cluster.es.example.com"
//! api_key = { env = "LAMBO_ES_API_KEY" }   # by reference only, never inline
//! index_prefix = "lambo"
//! refresh = "false"                        # "wait_for" in tests only
//! ```
//!
//! **Why `[recall]` and not `[store.recall]`.** The issue sketched the table
//! under `[store]`. `StoreConfig` is a public struct with public fields that
//! dozens of call sites (and library consumers) build literally, so a new field
//! there breaks every one of them; a top-level `LamboFile` section does not, and
//! it reads the same to an operator. Recorded in
//! `dev-diary/notes/feature-18-elastic-tier.md`.

use serde::{Deserialize, Deserializer, Serialize};

use crate::types::{SessionId, StoreError};

/// Recall tier selector (`[recall] kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallKind {
    /// Elasticsearch kNN over a per-contract index. Feature: `recall-elastic`.
    Elastic,
}

impl RecallKind {
    /// The Cargo feature that compiles this tier in.
    pub const fn feature_name(self) -> &'static str {
        match self {
            Self::Elastic => "recall-elastic",
        }
    }

    /// Whether this tier's feature is compiled into the current binary.
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Elastic => cfg!(feature = "recall-elastic"),
        }
    }
}

impl std::fmt::Display for RecallKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Elastic => write!(f, "elastic"),
        }
    }
}

impl std::str::FromStr for RecallKind {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "elastic" | "elasticsearch" => Ok(Self::Elastic),
            other => Err(StoreError::Backend(format!(
                "unknown recall tier kind {other:?} (expected elastic)"
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for RecallKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: StoreError| serde::de::Error::custom(e.to_string()))
    }
}

/// A secret named by reference: `{ env = "NAME" }`.
///
/// There is deliberately no inline form. A bare string where a secret belongs
/// is refused at parse time, so an API key cannot end up in a committed
/// `lambo.toml` by way of a config this crate accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// Environment variable holding the secret. Read once, when the tier is
    /// built; an unset or empty variable is a construction error.
    pub env: String,
}

impl SecretRef {
    /// Read the referenced value. Never logged.
    pub fn resolve(&self) -> Result<String, StoreError> {
        match std::env::var(&self.env) {
            Ok(v) if !v.trim().is_empty() => Ok(v),
            _ => Err(StoreError::Backend(format!(
                "recall.api_key names environment variable {} but it is unset or empty",
                self.env
            ))),
        }
    }
}

/// When a mirror write becomes visible to search (`[recall] refresh`).
///
/// `"false"` (the default) leaves visibility to the index's
/// `refresh_interval` (1 s by default), which is the accepted staleness
/// divergence the tier documents. `"wait_for"` blocks each write until it is
/// searchable and is meant for tests; `"true"` forces a refresh per write and
/// is not meant for production at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecallRefresh {
    #[default]
    #[serde(rename = "false")]
    False,
    #[serde(rename = "true")]
    True,
    #[serde(rename = "wait_for")]
    WaitFor,
}

impl RecallRefresh {
    /// The `refresh` query parameter value.
    pub const fn as_param(self) -> &'static str {
        match self {
            Self::False => "false",
            Self::True => "true",
            Self::WaitFor => "wait_for",
        }
    }
}

fn default_index_prefix() -> String {
    "lambo".into()
}

/// `[recall]`: the recall tier beside the durable store (#18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallConfig {
    pub kind: RecallKind,
    /// Cluster base URL. Must not carry credentials (`user:pass@`): secrets
    /// are by reference only, through [`Self::api_key`].
    pub url: String,
    /// API key, by reference (`{ env = "NAME" }`). Absent for a cluster with
    /// security off (tests).
    #[serde(default)]
    pub api_key: Option<SecretRef>,
    /// Index name prefix. Data indices are `{prefix}-v-{contract hash}`, the
    /// sync markers live in `{prefix}-meta`.
    #[serde(default = "default_index_prefix")]
    pub index_prefix: String,
    #[serde(default)]
    pub refresh: RecallRefresh,
    /// Per-request timeout in milliseconds (default 5000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// What `lambo recall-index backfill` did for one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecallBackfillReport {
    pub session: SessionId,
    /// Concept vectors written to the recall index.
    pub indexed: u64,
    /// The data index the vectors went to, or `None` when the session has no
    /// embedding contract (nothing to index).
    pub index: Option<String>,
    /// The durable mutation epoch the index now reflects.
    pub mutation_epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> Result<RecallConfig, toml::de::Error> {
        toml::from_str(toml_text)
    }

    #[test]
    fn a_minimal_section_takes_the_defaults() {
        let cfg = parse("kind = \"elastic\"\nurl = \"http://127.0.0.1:9200\"\n").unwrap();
        assert_eq!(cfg.kind, RecallKind::Elastic);
        assert_eq!(cfg.index_prefix, "lambo");
        assert_eq!(cfg.refresh, RecallRefresh::False);
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.timeout_ms, None);
    }

    #[test]
    fn the_secret_is_by_reference_only() {
        let by_ref = parse(
            "kind = \"elastic\"\nurl = \"http://h\"\napi_key = { env = \"LAMBO_TEST_ES\" }\n",
        )
        .unwrap();
        assert_eq!(by_ref.api_key.unwrap().env, "LAMBO_TEST_ES");
        let inline = parse("kind = \"elastic\"\nurl = \"http://h\"\napi_key = \"abc\"\n");
        assert!(inline.is_err(), "an inline secret must not parse");
    }

    #[test]
    fn unknown_keys_and_kinds_are_refused() {
        assert!(parse("kind = \"elastic\"\nurl = \"http://h\"\nindx_prefix = \"x\"\n").is_err());
        let err = parse("kind = \"solr\"\nurl = \"http://h\"\n").unwrap_err();
        assert!(
            err.to_string().contains("unknown recall tier kind"),
            "{err}"
        );
        assert!(parse("url = \"http://h\"\n").is_err(), "kind is required");
    }

    #[test]
    fn refresh_spellings_match_the_query_parameter() {
        for (text, want) in [
            ("false", RecallRefresh::False),
            ("true", RecallRefresh::True),
            ("wait_for", RecallRefresh::WaitFor),
        ] {
            let cfg = parse(&format!(
                "kind = \"elastic\"\nurl = \"http://h\"\nrefresh = \"{text}\"\n"
            ))
            .unwrap();
            assert_eq!(cfg.refresh, want);
            assert_eq!(cfg.refresh.as_param(), text);
        }
    }
}
