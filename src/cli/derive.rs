//! `lambo derive` — lease-held thin adapter over [`crate::memory::Memory::derive`].

use super::caps::{check_size_cli, require_nonempty, CliError, ConceptKind};
use super::{close_writer, open_writer};
use crate::graph::cite::{MAX_AGAINST_PER_CONCEPT, MAX_REFERENCES_PER_CONCEPT};
use crate::graph::derive::ParentOf;
use crate::graph::hybrid;
use crate::resolve::ResolvedBackends;
use crate::surface::validate::{check_concept_count, check_no_image_suffix};
use crate::types::{ConceptCitations, ConceptType};

/// Parsed `derive` flags.
pub struct Args {
    pub repo_scope: Option<String>,
    pub session: String,
    pub agent: String,
    pub content: String,
    pub kind: ConceptKind,
    pub parent_of: Vec<String>,
    pub concept: Vec<String>,
    /// `--depends-on` (#36 PR 4): existing concepts the first concept rests
    /// on.
    pub depends_on: Vec<String>,
    /// `--against` (#19(b)): existing concepts the first concept
    /// contradicts.
    pub against: Vec<String>,
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
    if let Some(scope) = args.repo_scope.as_deref() {
        crate::graph::resource::check_repo_scope(scope)
            .map_err(|e| CliError::Usage(e.replacen("repo_scope", "--repo-scope", 1)))?;
    }
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
    check_concept_count(concepts.len()).map_err(CliError::Usage)?;
    for (content, _) in &concepts {
        check_size_cli("concept.content", content)?;
        require_nonempty("concept.content", content)?;
        // #22 PR 4: only `lambo derive-image` builds an image suffix.
        check_no_image_suffix("concept.content", content).map_err(CliError::Usage)?;
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    for raw in &args.parent_of {
        check_size_cli("parent-of", raw)?;
        let (parent, child) = parse_parent_of(raw)?;
        check_size_cli("parent_of.parent", &parent)?;
        check_size_cli("parent_of.child", &child)?;
        pairs.push((parent, child));
    }

    // #36 PR 4: the first concept's citations, shape-checked before the
    // writer opens; whether each resolves is the core's question.
    if args.depends_on.len() > MAX_REFERENCES_PER_CONCEPT {
        return Err(CliError::Usage(format!(
            "depends-on may be given at most {MAX_REFERENCES_PER_CONCEPT} times"
        )));
    }
    for r in &args.depends_on {
        require_nonempty("depends-on", r)?;
        check_size_cli("depends-on", r)?;
    }
    // #19(b): the first concept's contradictions, the same way.
    if args.against.len() > MAX_AGAINST_PER_CONCEPT {
        return Err(CliError::Usage(format!(
            "against may be given at most {MAX_AGAINST_PER_CONCEPT} times"
        )));
    }
    for r in &args.against {
        require_nonempty("against", r)?;
        check_size_cli("against", r)?;
    }
    let citations: Vec<ConceptCitations> = if args.depends_on.is_empty() && args.against.is_empty()
    {
        Vec::new()
    } else {
        vec![ConceptCitations {
            content: args.content.clone(),
            depends_on: args.depends_on.clone(),
            against: args.against.clone(),
        }]
    };

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
    }
    .with_repo_scope(args.repo_scope.as_deref());
    // #74: a usage error naming the limit, before the write, on a session
    // that embeds (the core refuses it too, as a runtime error).
    if mem.derive_embeds()
        && let Err(msg) = hybrid::check_embed_context(&refs, &parent_of, None)
    {
        return close_writer(mem, Err(CliError::Usage(msg))).await;
    }
    let out = match mem
        .derive_cited_as(mem.agent(), &refs, &parent_of, &citations)
        .await
    {
        Ok(outcome) => {
            let mut summary = format!(
                "derived {} concept(s): {} created, {} matched existing",
                concepts.len(),
                outcome.created.len(),
                outcome.matched.len()
            );
            // #36 PR 4: said only when the call cited something, so a
            // derive without citations prints exactly what it printed.
            if !args.depends_on.is_empty() {
                summary.push_str(&format!("; {} citation(s) recorded", outcome.cited));
            }
            // #19(b): likewise for contradictions.
            if !args.against.is_empty() {
                summary.push_str(&format!(
                    "; {} contradiction(s) recorded",
                    outcome.contradicted
                ));
            }
            // #198 part c: said only when the write listed candidates, so
            // every other derive prints what it printed.
            if let Some(report) =
                crate::graph::suspected::replacement::ReplacementReport::for_receipt(
                    &outcome.replacement_candidates,
                )
            {
                summary.push_str(&report.describe());
            }
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
        assert!(matches!(parse_parent_of("child:"), Err(CliError::Usage(_))));
    }
}
