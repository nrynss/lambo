use std::collections::BTreeMap;

use super::plan::ClassificationRules;
use crate::types::{Concept, ConceptType};

pub(super) type Scores = BTreeMap<String, u32>;

/// Anchored Unicode-character glob with linear backtracking over a `*`.
/// No regex metacharacters or filesystem traversal are interpreted.
pub(super) fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<_> = pattern.chars().collect();
    let t: Vec<_> = text.chars().collect();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(at) = star {
            retry += 1;
            j = retry;
            i = at + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

pub(super) fn phrase(text: &str, needle: &str) -> bool {
    let text = text.to_lowercase();
    let needle = needle.to_lowercase();
    text.match_indices(&needle).any(|(at, _)| {
        let left = text[..at].chars().next_back();
        let right = text[at + needle.len()..].chars().next();
        !left.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !right.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

fn folder(text: &str, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    text.match_indices(path).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + path.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '/' | '_' | '-'))
            && !after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
    })
}

pub(super) fn actor(rules: &ClassificationRules, agent: &str) -> Scores {
    rules
        .projects
        .iter()
        .filter(|p| p.agent_id_patterns.iter().any(|pat| glob(pat, agent)))
        .map(|p| (p.session.clone(), 4))
        .collect()
}

pub(super) fn content(rules: &ClassificationRules, text: &str) -> (Scores, Vec<String>) {
    let mut scores = Scores::new();
    let mut signals = Vec::new();
    for p in &rules.projects {
        let folders: Vec<_> = p
            .folders
            .iter()
            .enumerate()
            .filter(|(_, path)| folder(text, path))
            .map(|(n, _)| n)
            .collect();
        let keywords: Vec<_> = p
            .content_keywords
            .iter()
            .enumerate()
            .filter(|(_, key)| phrase(text, key))
            .map(|(n, _)| n)
            .collect();
        let score = keywords.len().min(3) as u32 + if folders.is_empty() { 0 } else { 6 };
        if score > 0 {
            scores.insert(p.session.clone(), score);
            signals.extend(
                folders
                    .into_iter()
                    .map(|n| format!("folder:{}:{n}", p.session)),
            );
            signals.extend(
                keywords
                    .into_iter()
                    .map(|n| format!("content:{}:{n}", p.session)),
            );
        }
    }
    (scores, signals)
}

pub(super) fn actor_signals(rules: &ClassificationRules, agent: &str) -> Vec<String> {
    rules
        .projects
        .iter()
        .flat_map(|p| {
            p.agent_id_patterns
                .iter()
                .enumerate()
                .filter(move |(_, pat)| glob(pat, agent))
                .map(move |(n, _)| format!("agent:{}:{n}", p.session))
        })
        .collect()
}

pub(super) fn merge(into: &mut Scores, other: &Scores) {
    for (key, score) in other {
        *into.entry(key.clone()).or_default() += score;
    }
}

pub(super) fn ranked(scores: &Scores) -> Vec<String> {
    let mut rows: Vec<_> = scores.iter().collect();
    rows.sort_by(|(ka, a), (kb, b)| b.cmp(a).then_with(|| ka.cmp(kb)));
    rows.into_iter().map(|(key, _)| key.clone()).collect()
}

pub(super) fn unique(scores: &Scores) -> Option<&str> {
    let mut rows: Vec<_> = scores.iter().collect();
    rows.sort_by(|(ka, a), (kb, b)| b.cmp(a).then_with(|| ka.cmp(kb)));
    let (top, value) = *rows.first()?;
    if rows.get(1).is_some_and(|(_, second)| *second == value) {
        None
    } else {
        Some(top.as_str())
    }
}

pub(super) fn has_reference(text: &str) -> bool {
    if text.contains(['/', '\\']) {
        return true;
    }
    let words: Vec<_> = text.split_whitespace().collect();
    for (i, raw) in words.iter().enumerate() {
        let token = raw
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '#' && c != '-')
            .to_ascii_lowercase();
        if token.starts_with('#') && token[1..].bytes().any(|b| b.is_ascii_digit()) {
            return true;
        }
        if matches!(
            token.as_str(),
            "pr" | "pull-request" | "issue" | "commit" | "branch"
        ) && words.get(i + 1).is_some()
        {
            return true;
        }
        if (7..=40).contains(&token.len())
            && token.bytes().all(|b| b.is_ascii_hexdigit())
            && token.bytes().any(|b| b.is_ascii_digit())
        {
            return true;
        }
        if token.split('.').any(|s| {
            matches!(
                s,
                "rs" | "go" | "py" | "sh" | "md" | "mdx" | "json" | "yaml" | "yml" | "toml"
            )
        }) {
            return true;
        }
        let version = token.trim_start_matches('v');
        if version.split_once('.').is_some_and(|(a, b)| {
            !a.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.starts_with(|c: char| c.is_ascii_digit())
        }) {
            return true;
        }
    }
    false
}

pub(super) fn operator_signal(rules: &ClassificationRules, text: &str) -> bool {
    rules.operator_wide_phrases.iter().any(|p| phrase(text, p))
}

pub(super) fn general_ruling(rules: &ClassificationRules, c: &Concept) -> bool {
    matches!(c.concept_type, ConceptType::Constraint | ConceptType::Logic)
        && operator_signal(rules, &c.content)
        && !has_reference(&c.content)
        && content(rules, &c.content).0.is_empty()
}
