//! Repository-scoped local path identity (#129). No filesystem access.

use unicode_normalization::UnicodeNormalization;

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::Graph;
use crate::types::{ConceptType, LamboError};

/// Disjoint from legacy canonical keys: their Unicode lowercase normalization
/// cannot produce the ASCII uppercase letters in this prefix.
pub const RESOURCE_KEY_PREFIX: &str = "RepoResource:v1:";

/// Check a caller-asserted repository identity without interpreting it:
/// nonempty after trimming, and within the per-string cap and text-safety
/// rules every other string field meets. The MCP and CLI surfaces report the
/// message as a parameter error.
pub fn check_repo_scope(scope: &str) -> Result<(), String> {
    if scope.trim().is_empty() {
        return Err("repo_scope must not be empty or whitespace".into());
    }
    crate::surface::validate::check_size("repo_scope", scope)
}

/// [`check_repo_scope`] for the library write paths, which refuse a bad
/// scope as [`LamboError::Config`] before anything is written.
pub fn validate_repo_scope(scope: Option<&str>) -> Result<(), LamboError> {
    scope.map_or(Ok(()), |s| check_repo_scope(s).map_err(LamboError::Config))
}

/// A local path, including the basename forms recognized by the shared-layer
/// refusal. Explicit roots also count. URI, scp and domain-led remote names do
/// not. Whitespace-bearing relative paths require `./`, `../` or a root prefix.
/// Slash-separated names in explicit Resource context count as paths.
pub fn is_path_like(content: &str) -> bool {
    let s = content.trim();
    if s.is_empty()
        || s.chars().any(char::is_control)
        || s.contains("://")
        || crate::surface::validate::is_scp_like(s)
    {
        return false;
    }
    let b = s.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    if s.starts_with(['/', '\\', '~'])
        || drive
        || ["./", "../", ".\\", "..\\"]
            .iter()
            .any(|prefix| s.starts_with(prefix))
    {
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

/// Collision-free tuple encoding, with case and path component order intact.
/// No stemming, synonyms or filesystem normalization applies.
pub fn scoped_resource_key(content: &str, repo_scope: Option<&str>) -> Option<String> {
    let scope = repo_scope?;
    if !is_path_like(content) {
        return None;
    }
    let scope: String = scope.trim().nfc().collect();
    let path: String = content.trim().nfc().collect();
    Some(format!(
        "{RESOURCE_KEY_PREFIX}{}",
        serde_json::to_string(&[scope, path]).expect("strings serialize")
    ))
}

pub(crate) fn canonicalize_resource(
    content: &str,
    graph: &Graph,
    kind: ConceptType,
    repo_scope: Option<&str>,
) -> Result<CanonicalizeResult, LamboError> {
    if kind == ConceptType::Resource
        && let Some(key) = scoped_resource_key(content, repo_scope)
    {
        return Ok(match graph.concept_by_key(&key) {
            Some(node) => CanonicalizeResult::Matched { key, node },
            None => CanonicalizeResult::Unmatched { key },
        });
    }
    canonicalize(content, graph)
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
        ] {
            assert!(!is_path_like(prose), "{prose:?}");
        }
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
        for scope in ["", " \t\n", "a\0b", "a\u{202e}b"] {
            assert!(validate_repo_scope(Some(scope)).is_err(), "{scope:?}");
        }
        assert!(validate_repo_scope(Some(&"a".repeat(16_385))).is_err());
        assert!(validate_repo_scope(None).is_ok());
    }
}
