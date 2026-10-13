//! Repository-scoped local path identity (#129). No filesystem access.

use unicode_normalization::UnicodeNormalization;

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::Graph;
use crate::types::{ConceptType, LamboError, Node, NodeId};

/// Disjoint from legacy canonical keys: their Unicode lowercase normalization
/// cannot produce the ASCII uppercase letters in this prefix.
pub const RESOURCE_KEY_PREFIX: &str = "RepoResource:v1:";

/// The longest `repo_scope`, in bytes. A scoped key holds the scope and the
/// path, and the stores' unique `(session, canonical_key)` btree index has a
/// per-entry size limit (about 2.7 KB in Postgres). The cap keeps the scope
/// from being what overflows it; it does not bound the entry, since the
/// path can still be as long as any content, exactly as a legacy key can.
/// A real remote id (`host/owner/repo`) is far shorter.
pub const MAX_REPO_SCOPE_BYTES: usize = 512;

/// Invisible format characters a scope may not hold anywhere: zero-width
/// space, non-joiner and joiner, word joiner, byte-order mark, soft hyphen,
/// and the bidirectional marks and controls. Two scopes that differ only by
/// one would look alike and key differently.
const INVISIBLE: &[char] = &[
    '\u{00AD}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}',
    '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2060}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{FEFF}',
];

/// Check a caller-asserted repository identity without interpreting it:
/// nonempty after trimming, at most [`MAX_REPO_SCOPE_BYTES`], and with no
/// whitespace, control or invisible format character inside (the ends are
/// trimmed). The MCP and CLI surfaces report the message as a parameter
/// error.
pub fn check_repo_scope(scope: &str) -> Result<(), String> {
    let trimmed = scope.trim();
    if trimmed.is_empty() {
        return Err("repo_scope must not be empty or whitespace".into());
    }
    if scope.len() > MAX_REPO_SCOPE_BYTES {
        return Err(format!(
            "repo_scope exceeds {MAX_REPO_SCOPE_BYTES} bytes ({} given); send a short \
             stable repository id such as host/owner/repo",
            scope.len()
        ));
    }
    if let Some(c) = trimmed
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || INVISIBLE.contains(c))
    {
        return Err(format!(
            "repo_scope contains a disallowed character (U+{:04X}): a repository id has no \
             whitespace, control or invisible characters",
            c as u32
        ));
    }
    crate::surface::validate::check_size("repo_scope", scope)
}

/// [`check_repo_scope`] for the library write paths, which refuse a bad
/// scope as [`LamboError::Config`] before anything is written.
pub fn validate_repo_scope(scope: Option<&str>) -> Result<(), LamboError> {
    scope.map_or(Ok(()), |s| check_repo_scope(s).map_err(LamboError::Config))
}

/// The check the apply path makes (the graph's write functions, and a
/// stored intent at replay): a given scope is nonempty after trimming. The
/// full [`check_repo_scope`] runs once, on the call path (the MCP and CLI
/// surfaces and every `Memory` write entry point), before the ack; a row a
/// build accepted is not re-judged by a later build's stricter rules, which
/// would fail a write already acknowledged.
pub(crate) fn require_repo_scope(scope: Option<&str>) -> Result<(), LamboError> {
    let Some(scope) = scope else { return Ok(()) };
    if scope.trim().is_empty() {
        return Err(LamboError::Config(
            "a repo_scoped intent carries an empty repo_scope".into(),
        ));
    }
    Ok(())
}

/// `s` starts with `prefix` and a non-whitespace character follows it: `/`
/// alone, `/ note` and `~ 5 minutes` are not anchored paths.
fn anchored(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| !c.is_whitespace())
}

/// An explicitly anchored local path: `./`, `../`, `.\`, `..\`, a root `/` or
/// `\`, `~/` or `~\`, or a drive (`C:\`, `C:/`), each followed by a
/// non-whitespace character. The anchor is what lets such a path hold spaces.
fn is_anchored_path(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = b.len() >= 4
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && matches!(b[2], b'/' | b'\\')
        && !(b[3] as char).is_whitespace();
    drive
        || ["./", "../", ".\\", "..\\", "~/", "~\\", "/", "\\"]
            .iter()
            .any(|p| anchored(s, p))
}

/// The text a predicate looks at: trimmed, without a trailing line reference
/// (`:12`, `#L12`), as the shared-layer refusal reads it.
fn path_text(content: &str) -> &str {
    crate::surface::validate::strip_line_reference(content.trim())
}

fn rejected(s: &str) -> bool {
    // Only separators (`/`, `\\`) is no path.
    s.trim_matches(['/', '\\']).is_empty()
        || s.chars().any(char::is_control)
        || s.contains("://")
        || crate::surface::validate::is_scp_like(s)
}

/// A local path in an **explicit Resource** context (a `resource` concept,
/// an action's `produces` and `modifies`): an anchored path (see
/// [`is_anchored_path`]); else, with no whitespace, a separated name whose
/// first segment is not domain-like (`src/main.rs`, `docs/reference`), or a
/// lone file name the shared-layer refusal recognizes (`README.md`,
/// `Makefile`, `.env`). URI, scp and domain-led remote names are not paths.
/// Slash prose (`TCP/IP`) passes here, where the caller said Resource; see
/// [`is_inferred_path`] for text whose type is inferred.
pub fn is_path_like(content: &str) -> bool {
    let s = path_text(content);
    if rejected(s) {
        return false;
    }
    if is_anchored_path(s) {
        return true;
    }
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    if s.contains(['/', '\\']) {
        let first = s.split(['/', '\\']).next().unwrap_or(s);
        // Domain-led remote names need an explicit ./ to be local.
        return !first.trim_start_matches('.').contains('.');
    }
    crate::surface::validate::is_bare_relative_path(s)
}

/// A local path where the type is **inferred** (an action `depends_on`
/// entry, a `parent_of` end the call does not declare, a reference): only
/// an anchored path or what the shared-layer refusal calls a bare relative
/// path (a file name, a dotfile, a known extensionless file, a directory
/// with a trailing separator). `TCP/IP`, `CI/CD`, `I/O`, `and/or` and a
/// directory named without a trailing separator (`docs/reference`) are not.
pub fn is_inferred_path(content: &str) -> bool {
    let s = path_text(content);
    !rejected(s) && (is_anchored_path(s) || crate::surface::validate::is_bare_relative_path(s))
}

/// The scoped key of `content` in `repo_scope`, whatever the predicates say:
/// trimmed NFC scope and path; the path without a trailing line reference
/// (`:12`, `#L12`), so `src/main.rs:12` and `src/main.rs` are one file; the
/// path's trailing separators folded so `docs/` and `docs` are one
/// directory. Callers decide whether the content is a scoped path. The
/// concept's content keeps the spelling its first write used.
pub(crate) fn scoped_key_unchecked(content: &str, repo_scope: &str) -> String {
    let scope: String = repo_scope.trim().nfc().collect();
    let mut path: String = path_text(content).nfc().collect();
    while path.len() > 1 && path.ends_with(['/', '\\']) {
        path.pop();
    }
    format!(
        "{RESOURCE_KEY_PREFIX}{}",
        serde_json::to_string(&[scope, path]).expect("strings serialize")
    )
}

/// Collision-free tuple encoding, with case and path component order intact.
/// No stemming, synonyms or filesystem normalization applies. `None` without
/// a scope or for content that is not [`is_path_like`].
pub fn scoped_resource_key(content: &str, repo_scope: Option<&str>) -> Option<String> {
    let scope = repo_scope?;
    is_path_like(content).then(|| scoped_key_unchecked(content, scope))
}

/// The `(repo_scope, path)` a [`scoped_resource_key`] encodes, or `None` for
/// any other key (a legacy canonical key, or one this build cannot parse).
pub fn scoped_key_parts(key: &str) -> Option<(String, String)> {
    let tuple = key.strip_prefix(RESOURCE_KEY_PREFIX)?;
    let [repo, path]: [String; 2] = serde_json::from_str(tuple).ok()?;
    Some((repo, path))
}

/// The repository a scoped key names, for display (recall labels, ambiguity
/// candidates). `None` for an unscoped concept.
pub fn repo_scope_of_key(key: &str) -> Option<String> {
    scoped_key_parts(key).map(|(repo, _)| repo)
}

/// Whether `key` is a repository-scoped key.
pub fn is_scoped_key(key: &str) -> bool {
    key.starts_with(RESOURCE_KEY_PREFIX)
}

/// The lowercase spelling of [`RESOURCE_KEY_PREFIX`], for [`tie_form`].
const TIE_PREFIX: &str = "reporesource:v1:";

/// A key's form in the canonical-key tie order (issue #2). A legacy key is
/// itself. A scoped key's ASCII prefix is lowercased, so the uppercase
/// prefix no longer sorts every scoped key ahead of every legacy one; the
/// stores' SQL computes the same bytes (`'reporesource:v1:' ||
/// substr(canonical_key, 17)`).
pub fn tie_form(key: &str) -> std::borrow::Cow<'_, str> {
    match key.strip_prefix(RESOURCE_KEY_PREFIX) {
        Some(rest) => std::borrow::Cow::Owned(format!("{TIE_PREFIX}{rest}")),
        None => std::borrow::Cow::Borrowed(key),
    }
}

/// The SQL spelling of [`tie_form`] over a `canonical_key` column, shared by
/// the SQLite and Postgres keyword legs.
pub const TIE_FORM_SQL: &str = "CASE WHEN substr(canonical_key, 1, 16) = 'RepoResource:v1:' \
     THEN 'reporesource:v1:' || substr(canonical_key, 17) ELSE canonical_key END";

/// Whether the keyword leg may match tokens inside `key`. A scoped key's
/// text is a fixed prefix and a JSON tuple, so substring-matching it would
/// let `repo` or `resource` match every scoped path; its content (the path)
/// is matched instead.
pub fn keyword_matches_key(key: &str) -> bool {
    !is_scoped_key(key)
}

/// The SQL guard for [`keyword_matches_key`].
pub const KEYWORD_KEY_GUARD_SQL: &str = "substr(canonical_key, 1, 16) <> 'RepoResource:v1:'";

/// The concept `content` names under its legacy key, when that concept is
/// not a Resource: a Logic `config.toml` or an Entity `TCP/IP` stays
/// reachable by text under a scope, while an unscoped path Resource (a hub
/// several repositories may have joined) does not.
pub(crate) fn legacy_non_resource(
    content: &str,
    graph: &Graph,
) -> Result<Option<NodeId>, LamboError> {
    Ok(match canonicalize(content, graph)? {
        CanonicalizeResult::Matched { node, .. } => match graph.node(node) {
            Some(Node::Concept(c)) if c.concept_type != ConceptType::Resource => Some(node),
            _ => None,
        },
        CanonicalizeResult::Unmatched { .. } => None,
    })
}

/// How inferred text keys under a scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inferred {
    /// The scoped key: an existing or planned scoped path, or a new one.
    Scoped(String),
    /// The legacy key: an existing non-Resource concept, or new prose.
    Legacy,
    /// Both an existing scoped path and an existing non-Resource concept
    /// answer to the text: the caller refuses it as ambiguous.
    Ambiguous(Vec<NodeId>),
}

/// The identity of text whose type is inferred (#129 review M1), under
/// `repo_scope`: the scoped path when one exists (in `graph`, or `planned`
/// earlier in the same call), a non-Resource concept under the legacy key,
/// [`Inferred::Ambiguous`] when both exist; with neither, a new scoped path
/// if the text [`is_inferred_path`], else new legacy text.
pub(crate) fn infer_identity(
    content: &str,
    graph: &Graph,
    repo_scope: &str,
    planned: impl Fn(&str) -> Option<NodeId>,
) -> Result<Inferred, LamboError> {
    let scoped = is_path_like(content).then(|| scoped_key_unchecked(content, repo_scope));
    let scoped_hit = scoped
        .as_deref()
        .and_then(|k| graph.concept_by_key(k).or_else(|| planned(k)));
    let legacy_hit = legacy_non_resource(content, graph)?;
    Ok(match (scoped_hit, legacy_hit) {
        (Some(a), Some(b)) => Inferred::Ambiguous(vec![a, b]),
        (Some(_), None) => Inferred::Scoped(scoped.expect("a hit has a key")),
        (None, Some(_)) => Inferred::Legacy,
        (None, None) if is_inferred_path(content) => {
            Inferred::Scoped(scoped_key_unchecked(content, repo_scope))
        }
        (None, None) => Inferred::Legacy,
    })
}

/// Canonicalize `content` under an already decided identity: the scoped key
/// when `scoped_key` is given (exact lookup, no synonyms), else the legacy
/// canonicalizer.
pub(crate) fn canonicalize_keyed(
    content: &str,
    graph: &Graph,
    scoped_key: Option<String>,
) -> Result<CanonicalizeResult, LamboError> {
    match scoped_key {
        Some(key) => Ok(match graph.concept_by_key(&key) {
            Some(node) => CanonicalizeResult::Matched { key, node },
            None => CanonicalizeResult::Unmatched { key },
        }),
        None => canonicalize(content, graph),
    }
}

/// The explicit-context identity: a Resource that [`is_path_like`] under a
/// scope takes its scoped key; anything else its legacy key.
///
/// An explicit Resource never takes an existing plain concept's legacy key,
/// even for loosely path-like text (`scripts/build`), because that concept
/// may be another repository's: joining it would re-join repositories.
/// So a loosely path-like name that an inferred `depends_on` met first (as
/// plain text) and a later `produces` (scoped) are two concepts; spelled as
/// a clear path (`./scripts/build`, `scripts/build/`) both are scoped (#129
/// focused review L2).
pub(crate) fn canonicalize_resource(
    content: &str,
    graph: &Graph,
    kind: ConceptType,
    repo_scope: Option<&str>,
) -> Result<CanonicalizeResult, LamboError> {
    let key = if kind == ConceptType::Resource {
        scoped_resource_key(content, repo_scope)
    } else {
        None
    };
    canonicalize_keyed(content, graph, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::canonical::canonical_key;

    #[test]
    fn repo_path_predicate_is_syntactic_and_platform_independent() {
        for path in [
            "docs/reference",
            "src/main.rs",
            "README.md",
            "Makefile",
            ".env",
            "a\\b",
            "/tmp/file name",
            "./file name",
            "../a",
            "C:\\a b",
            "~/.config/a",
            "and/or",
            "./src.old/main.rs",
        ] {
            assert!(is_path_like(path), "{path:?}");
        }
        for prose in [
            "a decision",
            "file name.rs",
            "https://host/a.rs",
            "git@host:a.rs",
            "github.com/a/b",
            "src.old/a.rs",
            "example.com",
            "Node.js",
            "",
            "a\nb",
            // Review M1: an anchor needs a non-space character after it.
            "~ 5 minutes",
            "/ note",
            "/",
            "~",
        ] {
            assert!(!is_path_like(prose), "{prose:?}");
        }
        // Review L1: a line reference does not hide a path.
        for path in ["src/main.rs:12", "README.md:3:5", "README.md#L12-L20"] {
            assert!(is_path_like(path) && is_inferred_path(path), "{path:?}");
        }
    }

    /// Review M1: where the type is inferred, only a file-like name, a
    /// trailing-separator directory or an anchored path is a path; slash
    /// prose and a bare directory name are not.
    #[test]
    fn inferred_paths_exclude_slash_prose_and_bare_directories() {
        for path in [
            "README.md",
            "src/main.rs",
            "docs/reference/",
            ".env",
            "Makefile",
            "./run.sh",
            "/etc/hosts",
            "~/.config/a",
            "./file name",
        ] {
            assert!(is_inferred_path(path), "{path:?}");
        }
        for not in [
            "TCP/IP",
            "CI/CD",
            "I/O",
            "and/or",
            "docs/reference",
            "src",
            "~ 5 minutes",
            "/ note",
            "a decision",
        ] {
            assert!(!is_inferred_path(not), "{not:?}");
        }
        // Explicit Resource context still takes slash names.
        assert!(is_path_like("TCP/IP") && is_path_like("docs/reference"));
    }

    /// Review M3: a directory's trailing separators fold, so `docs/` and
    /// `docs` are one scoped key.
    #[test]
    fn directory_keys_fold_trailing_separators() {
        let k = |p: &str| scoped_key_unchecked(p, "r");
        assert_eq!(k("docs/"), k("docs"));
        assert_eq!(k("docs\\"), k("docs"));
        assert_eq!(k("docs//"), k("docs"));
        assert_ne!(k("/"), k(""));
        assert_eq!(
            scoped_resource_key("docs/reference/", Some("r")),
            Some(k("docs/reference"))
        );
        // Focused review L1: a trailing line reference names the file.
        for spelled in [
            "src/main.rs:12",
            "src/main.rs:12:5",
            "src/main.rs#L3-L5",
            " src/main.rs ",
        ] {
            assert_eq!(k(spelled), k("src/main.rs"), "{spelled}");
        }
    }

    /// Review L5: the tie form lowercases a scoped key's ASCII prefix, so
    /// scoped keys sort among legacy keys rather than ahead of all of them.
    #[test]
    fn tie_form_removes_the_uppercase_prefix_bias() {
        let scoped = scoped_key_unchecked("notes.md", "r");
        assert_eq!(tie_form("aaa notes"), "aaa notes");
        assert!(tie_form(&scoped).starts_with("reporesource:v1:"));
        assert!(*tie_form("aaa notes") < *tie_form(&scoped));
        assert!(*tie_form(&scoped) < *tie_form("zzz notes"));
        assert!(!keyword_matches_key(&scoped) && keyword_matches_key("aaa notes"));
    }

    #[test]
    fn structured_repo_keys_preserve_case_order_and_delimiter_boundaries() {
        let key = |repo: &str, path: &str| scoped_resource_key(path, Some(repo)).unwrap();
        assert_ne!(key("a", "src/a_b.rs"), key("a", "src/b_a.rs"));
        assert_ne!(key("a", "src/Main.rs"), key("a", "src/main.rs"));
        assert_ne!(key("a", "src/main.rs"), key("b", "src/main.rs"));
        assert_ne!(key("a\",\"src", "main.rs"), key("a", "src/main.rs"));
        assert_ne!(key("a", "./src/main.rs"), key("a", "src/main.rs"));
        assert_ne!(key("a", "/src/main.rs"), key("a", "src/main.rs"));
        let encoded = key("a", "src/main.rs");
        assert!(!canonical_key(&encoded, |_| None).starts_with(RESOURCE_KEY_PREFIX));
        let parts: Vec<String> =
            serde_json::from_str(encoded.strip_prefix(RESOURCE_KEY_PREFIX).unwrap()).unwrap();
        assert_eq!(parts, ["a", "src/main.rs"]);
        assert_eq!(
            scoped_key_parts(&encoded),
            Some(("a".to_string(), "src/main.rs".to_string()))
        );
        assert_eq!(repo_scope_of_key("main rs src"), None);
        assert_eq!(repo_scope_of_key("RepoResource:v1:not json"), None);
    }

    #[test]
    fn repo_keys_normalize_nfc_and_trim_without_interpreting_remotes() {
        assert_eq!(
            scoped_resource_key(" ./cafe\u{301}.rs ", Some(" re\u{301}po ")),
            scoped_resource_key("./café.rs", Some("répo"))
        );
        assert_ne!(
            scoped_resource_key("a.rs", Some("host/owner/repo.git")),
            scoped_resource_key("a.rs", Some("host/owner/repo"))
        );
        assert_eq!(scoped_resource_key("a.rs", None), None);
        assert_eq!(scoped_resource_key("a decision", Some("repo")), None);
    }

    #[test]
    fn invalid_repo_scope_is_refused() {
        // Review L3: no whitespace, control or invisible character inside.
        for scope in [
            "",
            " \t\n",
            "a\0b",
            "a\u{202e}b",
            "a b",
            "a\tb",
            "a\nb",
            "a\u{200C}b",
            "a\u{200D}b",
            "a\u{200B}b",
            "a\u{FEFF}b",
        ] {
            assert!(validate_repo_scope(Some(scope)).is_err(), "{scope:?}");
        }
        // Review L2: at most MAX_REPO_SCOPE_BYTES.
        assert!(check_repo_scope(&"a".repeat(MAX_REPO_SCOPE_BYTES)).is_ok());
        let long = check_repo_scope(&"a".repeat(MAX_REPO_SCOPE_BYTES + 1)).unwrap_err();
        assert!(long.contains("512 bytes"), "{long}");
        assert!(validate_repo_scope(Some(" github.com/acme/widgets ")).is_ok());
        assert!(validate_repo_scope(None).is_ok());
        // Replay checks only that a stored scope is nonempty (review L4).
        assert!(require_repo_scope(Some("a b")).is_ok());
        assert!(require_repo_scope(Some("  ")).is_err());
    }
}
