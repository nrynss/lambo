//! `[web]` in `lambo.toml`: the read-only portal's served sessions, its
//! `Host` allow-list and its view bounds (#4).
//!
//! `lambo serve-web` serves an explicit allowlist of sessions: the ordered
//! union of the repeatable `--session` and `sessions` here, the first being
//! the default the unscoped routes alias (#4 PR 2). Each session is read
//! through a per-session **reader view**: one store load, shared by every
//! request and tab until it is older than `view_ttl_ms` (#4 PR 1). Every key
//! is optional, and an absent table is [`WebConfig::default`]: the defaults
//! below.
//!
//! ```toml
//! [web]
//! sessions = ["lambo", "rustydocs"]   # beside --session; the first is the default
//! allowed_hosts = ["lambo.example.com"]  # beside --allowed-host
//! view_ttl_ms = 1500          # 0 = a fresh load per request (still single-flight)
//! max_loaded_sessions = 4     # views held in memory at once
//! load_concurrency = 2        # simultaneous session loads (always 1 on SQLite)
//! recall_concurrency = 4      # simultaneous recalls; a request waiting 2 s gets 503
//! ```
//!
//! `allowed_hosts` matters only while no bearer token is configured: then
//! the portal answers only requests whose `Host` is `localhost`,
//! `127.0.0.1` or `[::1]` (any port) or one of these entries, which is its
//! DNS-rebinding defence. An entry is a host name or address, optionally
//! with `:port` (then only that port matches).
//!
//! # Level B rules
//!
//! The table is `deny_unknown_fields` and [`WebConfig::validate`] runs inside
//! [`crate::LamboFile::from_toml_str`], so a typo or an out-of-range value
//! stops every command that reads the file, like any other invalid key. An
//! older binary refuses a file that has `[web]` at all (unknown top-level
//! key), which is the existing Level B behaviour. A refusal of a numeric
//! key names the key and its bound, never the value (a wrong-typed value
//! is redacted by the shared parse-error path); a refused session name or
//! host is quoted, as `[serve]` quotes session names, since neither is a
//! secret.
//!
//! The strict addressed-id charset is not checked here: it applies only once
//! the union with `--session` serves more than one session (#4 Q12), which
//! `lambo serve-web` checks at startup. The approved design adds
//! `list_sessions` and `[[web.credential]]` in a later PR.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::LamboError;

/// A `Host` the portal accepts under the implicit loopback grant: a host
/// name or address, and a port when one was given (#4 PR 2, design 4.5).
/// Parsed from `[web] allowed_hosts` and `--allowed-host`; the loopback
/// names are built in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedHost {
    host: String,
    port: Option<u16>,
}

impl AllowedHost {
    /// Parse `host` or `host:port` (an IPv6 address in brackets). Refuses an
    /// empty entry, user info (`@`), a path, and anything else that is not
    /// an HTTP authority. The error names the rule, not the value.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let rule = "must be a host name or address, optionally with :port (no scheme, user \
                    info or path)";
        if raw.is_empty() || raw.contains('@') {
            return Err(rule.into());
        }
        let authority = raw
            .parse::<axum::http::uri::Authority>()
            .map_err(|_| rule.to_string())?;
        // `Authority` keeps a non-numeric port as text; refuse it here.
        let bad_port = raw
            .rsplit_once(']')
            .map_or(raw, |(_, tail)| tail)
            .contains(':')
            && authority.port_u16().is_none();
        if authority.host().is_empty() || bad_port {
            return Err(rule.into());
        }
        Ok(Self::from_parts(authority.host(), authority.port_u16()))
    }

    /// From a host and an optional port, normalized: ASCII lowercase, IPv6
    /// brackets removed.
    pub fn from_parts(host: &str, port: Option<u16>) -> Self {
        Self {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_ascii_lowercase(),
            port,
        }
    }

    /// Does a request's `Host` (already normalized by [`Self::from_parts`])
    /// match this entry? An entry without a port matches any port.
    pub fn matches(&self, presented: &AllowedHost) -> bool {
        self.host == presented.host && self.port.is_none_or(|p| presented.port == Some(p))
    }
}

/// Default view TTL: the page's poll interval (1.5 s), so every tab's poll
/// within one interval shares one load (#4 Q3).
pub const DEFAULT_VIEW_TTL_MS: u64 = 1_500;

/// Longest accepted view TTL (60 s). A view older than that is a frozen page
/// rather than a cache: the page polls every 1.5 s to look live.
pub const MAX_VIEW_TTL_MS: u64 = 60_000;

/// Default cap on session views held in memory (#4 Q17).
pub const DEFAULT_MAX_LOADED_SESSIONS: usize = 4;

/// Default bound on simultaneous session loads (#4 Q17). Forced to 1 on
/// SQLite, whose pool is one connection.
pub const DEFAULT_LOAD_CONCURRENCY: usize = 2;

/// Default bound on simultaneous recalls (#4 Q17).
pub const DEFAULT_RECALL_CONCURRENCY: usize = 4;

/// Largest accepted `load_concurrency` and `recall_concurrency`. Each sizes a
/// semaphore, and tokio's panics beyond `Semaphore::MAX_PERMITS`; a TOML
/// integer reaches far past that. Refusing at parse time keeps an absurd
/// value a config error instead of a startup panic, and 1024 is already far
/// more parallel loads or embeds than one portal process can use.
pub const MAX_CONCURRENCY: usize = 1_024;

/// `[web]`. Every key is optional; an absent table changes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Sessions to serve, beside the repeatable `--session` (#4 PR 2). The
    /// ordered union is the allowlist; its first entry is the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    /// Extra `Host` values accepted while no bearer token is configured,
    /// beside `--allowed-host` and the built-in loopback names (#4 PR 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
    /// Milliseconds a loaded session view is served before the next request
    /// reloads it. Default [`DEFAULT_VIEW_TTL_MS`]; 0 to [`MAX_VIEW_TTL_MS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_ttl_ms: Option<u64>,
    /// Session views held in memory at once; the least recently used is
    /// dropped beyond it. Default [`DEFAULT_MAX_LOADED_SESSIONS`]; >= 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_loaded_sessions: Option<usize>,
    /// Simultaneous session loads. Default [`DEFAULT_LOAD_CONCURRENCY`];
    /// 1 to [`MAX_CONCURRENCY`]. SQLite always uses 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_concurrency: Option<usize>,
    /// Simultaneous recalls, process-wide. Default
    /// [`DEFAULT_RECALL_CONCURRENCY`]; 1 to [`MAX_CONCURRENCY`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_concurrency: Option<usize>,
}

fn web_err(msg: impl std::fmt::Display) -> LamboError {
    LamboError::Config(format!("lambo.toml [web]: {msg}"))
}

impl WebConfig {
    /// True when no key is set (the table is not serialized then).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Refuse out-of-range values, an empty, oversized or repeated session
    /// name, and a malformed host. Run by `LamboFile::from_toml_str`.
    pub fn validate(&self) -> Result<(), LamboError> {
        for (i, name) in self.sessions.iter().enumerate() {
            if name.trim().is_empty() {
                return Err(web_err("a sessions entry is empty"));
            }
            crate::surface::validate::check_size("a sessions entry", name).map_err(web_err)?;
            if self.sessions[..i].contains(name) {
                return Err(web_err(format!("sessions lists {name:?} twice")));
            }
        }
        for host in &self.allowed_hosts {
            AllowedHost::parse(host)
                .map_err(|e| web_err(format!("allowed_hosts entry {host:?} {e}")))?;
        }
        if self.view_ttl_ms.is_some_and(|ms| ms > MAX_VIEW_TTL_MS) {
            return Err(web_err(format!(
                "view_ttl_ms must be at most {MAX_VIEW_TTL_MS} (0 reloads on every request)"
            )));
        }
        if self.max_loaded_sessions == Some(0) {
            return Err(web_err("max_loaded_sessions must be >= 1"));
        }
        for (key, value) in [
            ("load_concurrency", self.load_concurrency),
            ("recall_concurrency", self.recall_concurrency),
        ] {
            if value.is_some_and(|n| !(1..=MAX_CONCURRENCY).contains(&n)) {
                return Err(web_err(format!(
                    "{key} must be between 1 and {MAX_CONCURRENCY}"
                )));
            }
        }
        Ok(())
    }

    /// The view TTL, default applied.
    pub fn view_ttl(&self) -> Duration {
        Duration::from_millis(self.view_ttl_ms.unwrap_or(DEFAULT_VIEW_TTL_MS))
    }

    /// The view cap, default applied.
    pub fn max_loaded_sessions(&self) -> usize {
        self.max_loaded_sessions
            .unwrap_or(DEFAULT_MAX_LOADED_SESSIONS)
    }

    /// The configured load bound, default applied. The portal forces 1 on
    /// SQLite whatever this says.
    pub fn load_concurrency(&self) -> usize {
        self.load_concurrency.unwrap_or(DEFAULT_LOAD_CONCURRENCY)
    }

    /// The recall bound, default applied.
    pub fn recall_concurrency(&self) -> usize {
        self.recall_concurrency
            .unwrap_or(DEFAULT_RECALL_CONCURRENCY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LamboFile;

    #[test]
    fn an_absent_table_is_the_defaults() {
        let file = LamboFile::from_toml_str("").expect("empty file");
        assert!(file.web.is_empty());
        assert_eq!(file.web.view_ttl(), Duration::from_millis(1_500));
        assert_eq!(file.web.max_loaded_sessions(), 4);
        assert_eq!(file.web.load_concurrency(), 2);
        assert_eq!(file.web.recall_concurrency(), 4);
    }

    #[test]
    fn every_key_parses() {
        let file = LamboFile::from_toml_str(
            "[web]\nview_ttl_ms = 0\nmax_loaded_sessions = 2\nload_concurrency = 3\n\
             recall_concurrency = 1\n",
        )
        .expect("valid [web]");
        assert_eq!(file.web.view_ttl(), Duration::ZERO);
        assert_eq!(file.web.max_loaded_sessions(), 2);
        assert_eq!(file.web.load_concurrency(), 3);
        assert_eq!(file.web.recall_concurrency(), 1);
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let err = LamboFile::from_toml_str("[web]\nview_ttl = 1500\n")
            .expect_err("unknown key")
            .to_string();
        assert!(err.contains("unknown field `view_ttl`"), "{err}");
    }

    #[test]
    fn out_of_range_values_are_refused_naming_the_key_not_the_value() {
        for (toml, key) in [
            ("[web]\nview_ttl_ms = 60001\n", "view_ttl_ms"),
            ("[web]\nmax_loaded_sessions = 0\n", "max_loaded_sessions"),
            ("[web]\nload_concurrency = 0\n", "load_concurrency"),
            ("[web]\nrecall_concurrency = 0\n", "recall_concurrency"),
            ("[web]\nload_concurrency = 1025\n", "load_concurrency"),
            ("[web]\nrecall_concurrency = 1025\n", "recall_concurrency"),
            // Parses as a usize, and would panic in `Semaphore::new`.
            (
                "[web]\nload_concurrency = 4000000000000000000\n",
                "load_concurrency",
            ),
            (
                "[web]\nrecall_concurrency = 4000000000000000000\n",
                "recall_concurrency",
            ),
        ] {
            let err = LamboFile::from_toml_str(toml).expect_err(toml).to_string();
            assert!(err.contains("[web]") && err.contains(key), "{err}");
            for value in ["60001", "1025", "4000000000000000000"] {
                assert!(!err.contains(value), "the value is not quoted: {err}");
            }
        }
        LamboFile::from_toml_str("[web]\nview_ttl_ms = 60000\n").expect("the bound itself");
        LamboFile::from_toml_str("[web]\nload_concurrency = 1024\nrecall_concurrency = 1024\n")
            .expect("the concurrency bound itself");
    }

    /// #4 PR 2: `sessions` and `allowed_hosts` parse, in order.
    #[test]
    fn sessions_and_allowed_hosts_parse_in_order() {
        let file = LamboFile::from_toml_str(
            "[web]\nsessions = [\"lambo\", \"rustydocs\", \"team notes\"]\n\
             allowed_hosts = [\"lambo.example.com\", \"10.0.0.5:8443\", \"[::1]:7710\"]\n",
        )
        .expect("valid [web]");
        assert_eq!(file.web.sessions, ["lambo", "rustydocs", "team notes"]);
        assert_eq!(file.web.allowed_hosts.len(), 3);
        assert!(!file.web.is_empty());
    }

    /// An empty, repeated or control-character session name and a host that
    /// is not an HTTP authority are refused, naming the key.
    #[test]
    fn a_bad_session_or_host_is_refused_naming_the_key() {
        for (toml, key) in [
            ("[web]\nsessions = [\"\"]\n", "sessions entry"),
            ("[web]\nsessions = [\"  \"]\n", "sessions entry"),
            ("[web]\nsessions = [\"a\", \"b\", \"a\"]\n", "twice"),
            ("[web]\nsessions = [\"a\\u0007b\"]\n", "control character"),
            ("[web]\nallowed_hosts = [\"\"]\n", "allowed_hosts"),
            ("[web]\nallowed_hosts = [\"user@host\"]\n", "allowed_hosts"),
            (
                "[web]\nallowed_hosts = [\"https://host\"]\n",
                "allowed_hosts",
            ),
            ("[web]\nallowed_hosts = [\"host/path\"]\n", "allowed_hosts"),
            (
                "[web]\nallowed_hosts = [\"host:notaport\"]\n",
                "allowed_hosts",
            ),
        ] {
            let err = LamboFile::from_toml_str(toml).expect_err(toml).to_string();
            assert!(err.contains("[web]") && err.contains(key), "{toml}: {err}");
        }
    }

    /// An entry without a port matches any port; with one, only that port.
    /// Matching is case-insensitive and ignores IPv6 brackets.
    #[test]
    fn an_allowed_host_matches_by_name_and_optional_port() {
        let any_port = AllowedHost::parse("Lambo.Example.com").expect("host");
        let one_port = AllowedHost::parse("lambo.example.com:8443").expect("host:port");
        let v6 = AllowedHost::parse("[::1]").expect("v6");
        let presented = |h: &str, p: Option<u16>| AllowedHost::from_parts(h, p);
        assert!(any_port.matches(&presented("lambo.example.com", None)));
        assert!(any_port.matches(&presented("LAMBO.example.com", Some(443))));
        assert!(!any_port.matches(&presented("evil.example.com", None)));
        assert!(one_port.matches(&presented("lambo.example.com", Some(8443))));
        assert!(!one_port.matches(&presented("lambo.example.com", Some(443))));
        assert!(!one_port.matches(&presented("lambo.example.com", None)));
        assert!(v6.matches(&presented("[::1]", Some(7710))));
    }

    #[test]
    fn a_wrong_typed_value_is_not_quoted() {
        let err = LamboFile::from_toml_str("[web]\nview_ttl_ms = \"a-pasted-value\"\n")
            .expect_err("a string where a number goes")
            .to_string();
        assert!(!err.contains("a-pasted-value"), "{err}");
    }

    #[test]
    fn an_empty_table_is_not_serialized() {
        let text = toml::to_string(&LamboFile::default()).expect("serialize");
        assert!(!text.contains("[web]"), "{text}");
    }

    /// The commented `[web]` block in `lambo.example.toml`, uncommented, is a
    /// valid table with the documented defaults; commented, it sets nothing.
    #[test]
    fn the_example_files_web_block_parses_when_uncommented() {
        let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/lambo.example.toml"));
        let begin = raw
            .find("# --- [web] example begins ---")
            .expect("begin marker");
        let end = raw
            .find("# --- [web] example ends ---")
            .expect("end marker");
        let block: String = raw[begin..end]
            .lines()
            .skip(1)
            .map(|l| l.strip_prefix("# ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        let web = LamboFile::from_toml_str(&block)
            .unwrap_or_else(|e| panic!("example [web] must parse: {e}"))
            .web;
        assert_eq!(web.sessions, ["lambo", "rustydocs"]);
        assert_eq!(web.allowed_hosts, ["lambo.example.com"]);
        assert_eq!(web.view_ttl_ms, Some(DEFAULT_VIEW_TTL_MS));
        assert_eq!(web.max_loaded_sessions, Some(DEFAULT_MAX_LOADED_SESSIONS));
        assert_eq!(web.load_concurrency, Some(DEFAULT_LOAD_CONCURRENCY));
        assert_eq!(web.recall_concurrency, Some(DEFAULT_RECALL_CONCURRENCY));
        assert!(LamboFile::from_toml_str(raw)
            .expect("example")
            .web
            .is_empty());
    }
}
