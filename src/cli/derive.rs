//! `lambo derive` — lease-held thin adapter over [`crate::memory::Memory::derive`].

use super::caps::{
    check_size_cli, require_nonempty, CliError, ConceptKind, MAX_CONCEPTS_PER_DERIVE,
};
use super::{close_writer, open_writer};
use crate::graph::derive::ParentOf;
use crate::resolve::ResolvedBackends;
use crate::types::ConceptType;

/// Parsed `derive` flags.
pub struct Args {
    pub session: String,
    pub agent: String,
    pub content: String,
    pub kind: ConceptKind,
    pub parent_of: Vec<String>,
    pub concept: Vec<String>,
}

/// `--parent-of CHILD:PARENT` → `(parent, child)` for [`ParentOf::from_pairs`].
///
/// The separator is the **first** colon. The child is everything to its left
/// (so it never contains a colon), and the parent is everything to its right —
/// free text that MAY itself contain colons, so an IPv6 CIDR parent like
/// `2001:db8::/32` is fully expressible. Extra colons are therefore **not**
/// refused: they belong to the free-text parent, not to the separator, and
/// refusing them is exactly what forced the launcher to pre-filter IPv6
/// clientside (`scripts/cloudops/_lambo.py::_refuse_colon`). A value must
/// still yield a non-empty child and parent.
pub(crate) fn parse_parent_of(raw: &str) -> Result<(String, String), CliError> {
    match raw.split_once(':') {
        Some((child, parent)) if !child.trim().is_empty() && !parent.trim().is_empty() => {
            Ok((parent.to_string(), child.to_string()))
        }
        Some(_) | None => Err(CliError::Usage(
            "parent-of must be CHILD:PARENT (child left of the first colon, parent right; \
             the parent may itself contain colons, e.g. an IPv6 CIDR like 2001:db8::/32)"
                .into(),
        )),
    }
}

/// `--concept CONTENT:KIND`. Kind is the token after the last colon.
pub(crate) fn parse_concept(raw: &str) -> Result<(String, ConceptType), CliError> {
    let (content, kind) = raw.rsplit_once(':').ok_or_else(|| {
        CliError::Usage("concept must be CONTENT:KIND (kind after the last colon)".into())
    })?;
    require_nonempty("concept.content", content)?;
    let kind = ConceptKind::parse_token(kind)?;
    Ok((content.to_string(), kind.into()))
}

/// Derive concepts from this interaction into session memory.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    require_nonempty("content", &args.content)?;
    check_size_cli("concept.content", &args.content)?;

    let mut concepts: Vec<(String, ConceptType)> = vec![(args.content.clone(), args.kind.into())];
    for raw in &args.concept {
        check_size_cli("concept", raw)?;
        concepts.push(parse_concept(raw)?);
    }
    if concepts.len() > MAX_CONCEPTS_PER_DERIVE {
        return Err(CliError::Usage(format!(
            "concepts must contain at most {MAX_CONCEPTS_PER_DERIVE} entries"
        )));
    }
    for (content, _) in &concepts {
        check_size_cli("concept.content", content)?;
        require_nonempty("concept.content", content)?;
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    for raw in &args.parent_of {
        check_size_cli("parent-of", raw)?;
        let (parent, child) = parse_parent_of(raw)?;
        check_size_cli("parent_of.parent", &parent)?;
        check_size_cli("parent_of.child", &child)?;
        pairs.push((parent, child));
    }

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let refs: Vec<(&str, ConceptType)> = concepts.iter().map(|(c, t)| (c.as_str(), *t)).collect();
    let pair_refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let parent_of = if pair_refs.is_empty() {
        ParentOf::none()
    } else {
        ParentOf::from_pairs(&pair_refs)
    };
    let out = match mem.derive(&refs, &parent_of).await {
        Ok(outcome) => {
            let summary = format!(
                "derived {} concept(s): {} created, {} matched existing",
                concepts.len(),
                outcome.created.len(),
                outcome.matched.len()
            );
            Ok(summary)
        }
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_child_left_parent_right() {
        let (parent, child) = parse_parent_of("auth middleware:user schema").unwrap();
        assert_eq!(parent, "user schema");
        assert_eq!(child, "auth middleware");
    }

    #[test]
    fn concept_splits_on_last_colon() {
        let (content, kind) = parse_concept("foo:bar:entity").unwrap();
        assert_eq!(content, "foo:bar");
        assert_eq!(kind, ConceptType::Entity);
    }

    #[test]
    fn bad_parent_of_is_usage() {
        assert!(matches!(
            parse_parent_of("nocolon"),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn parent_of_accepts_colon_bearing_parent_ipv6_roundtrip() {
        // T1 part 2 #4 / T3-1-P2-3 — an IPv6 CIDR parent carries colons; the
        // separator is the FIRST colon, so the parent may itself contain
        // colons and is never dropped or mis-parsed.
        let (parent, child) = parse_parent_of("api node:2001:db8::/32").unwrap();
        assert_eq!(parent, "2001:db8::/32");
        assert_eq!(child, "api node");
        // Round-trip the reverse (IPv6 as the parent via first-colon split).
        let (p2, c2) = parse_parent_of("cache:fe80::1:2:3:4").unwrap();
        assert_eq!(p2, "fe80::1:2:3:4");
        assert_eq!(c2, "cache");
    }

    #[test]
    fn parent_of_rejects_empty_side() {
        // An empty child or parent is still malformed and is refused loudly.
        assert!(matches!(
            parse_parent_of(":parent"),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            parse_parent_of("child:"),
            Err(CliError::Usage(_))
        ));
    }
}
