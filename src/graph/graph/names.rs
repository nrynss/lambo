//! Durable, normalized names and the existing-memory upgrade (#146).
//!
//! Names reserve a spelling for an immutable id even after its target is
//! collected. Legacy raw synonyms are an import format, never write references.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{invariant, Graph};
use crate::graph::canonical::canonical_key;
use crate::types::{
    ConceptName, ConceptType, EdgeType, LamboError, Mutation, Node, NodeId, ReferenceField,
    ReferenceRefusalKind, RetiredAlias,
};

/// A declaration refusal is a fact about input, so a replay settles it
/// failed rather than pausing the session's backlog as a store outage.
pub(crate) fn invalid_name(name: &str, reason: impl Into<String>) -> LamboError {
    crate::surface::resolve::refusal(
        ReferenceField::Name,
        None,
        name,
        ReferenceRefusalKind::InvalidName {
            reason: reason.into(),
        },
    )
}

/// Validate and normalize one name using the same algorithm as concept keys.
pub fn name_key(name: &str) -> Result<String, LamboError> {
    crate::surface::validate::check_size("name", name).map_err(|why| invalid_name(name, why))?;
    let key = canonical_key(name, |_| None);
    if key.is_empty()
        || uuid::Uuid::parse_str(name.trim()).is_ok()
        || (name.trim().len() == 8 && name.trim().bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid_name(
            name,
            "a concept name must have a nonempty normalized key and cannot be an id",
        ));
    }
    Ok(key)
}

impl Graph {
    /// Resolve a reserved name, including a binding whose target was deleted.
    pub fn name_target(&self, name: &str) -> Option<NodeId> {
        self.names
            .get(&canonical_key(name, |_| None))
            .map(|n| n.node_id)
    }

    pub fn names(&self) -> impl Iterator<Item = &ConceptName> {
        self.names.values()
    }

    /// A bounded, stable set of display names for recall and inspection.
    pub fn display_names(&self, target: NodeId) -> Vec<String> {
        self.names
            .values()
            .filter(|n| n.node_id == target)
            .take(3)
            .map(|n| n.name.clone())
            .collect()
    }

    pub fn retired_alias(&self, id: NodeId) -> Option<&RetiredAlias> {
        self.retired_aliases.get(&id.to_string())
    }

    pub fn retired_aliases(&self) -> impl Iterator<Item = &RetiredAlias> {
        self.retired_aliases.values()
    }

    /// Check a declaration without changing state. `None` is a not-yet-written target.
    pub(crate) fn validate_name(
        &self,
        name: &str,
        target: Option<NodeId>,
    ) -> Result<String, LamboError> {
        let key = name_key(name)?;
        if let Some(bound) = self.names.get(&key)
            && Some(bound.node_id) != target
        {
            return Err(crate::surface::resolve::refusal(
                ReferenceField::Name,
                None,
                name,
                ReferenceRefusalKind::NameTaken {
                    target: bound.node_id,
                },
            ));
        }
        // Observations also occupy the naming namespace, even though their
        // concept keys are deliberately excluded from the canonical-match index.
        if let Some(c) = self
            .concepts()
            .find(|c| c.canonical_key == key && Some(c.id) != target)
        {
            return Err(crate::surface::resolve::refusal(
                ReferenceField::Name,
                None,
                name,
                ReferenceRefusalKind::NameTaken { target: c.id },
            ));
        }
        Ok(key)
    }

    /// Bind a name to a live concept; an identical repeated binding is a no-op.
    pub fn declare_name(&mut self, name: &str, target: NodeId) -> Result<bool, LamboError> {
        if !matches!(self.node(target), Some(Node::Concept(_)))
            || self.retired_alias(target).is_some()
        {
            return Err(invalid_name(
                name,
                format!("target {target} is not an active concept in this session"),
            ));
        }
        let key = self.validate_name(name, Some(target))?;
        if self.names.contains_key(&key) {
            return Ok(false);
        }
        let record = ConceptName {
            session_id: self.session_id.clone(),
            name: name.trim().into(),
            name_key: key.clone(),
            node_id: target,
        };
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.names
                .entry(key.clone())
                .or_insert_with(|| self.names.get(&key).cloned());
        }
        self.names.insert(key, record.clone());
        self.append_mutation(Mutation::SetConceptName { name: record });
        Ok(true)
    }

    fn archive_alias(&mut self, retired: RetiredAlias) -> Result<(), LamboError> {
        let key = retired.concept.id.to_string();
        if self.retired_aliases.contains_key(&key) {
            return Err(invariant("alias already retired"));
        }
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.retired_aliases.entry(key.clone()).or_insert(None);
        }
        self.retired_aliases.insert(key, retired.clone());
        self.append_mutation(Mutation::RetireAlias { retired });
        Ok(())
    }
}

/// A deterministic summary; original records are retained in retired-alias history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameUpgradeReport {
    pub names_migrated: usize,
    pub aliases_retired: usize,
    pub citations_moved: usize,
    pub already_applied: usize,
    pub mutation_epoch: u64,
}

/// Upgrade surviving synonyms and a reviewed alias-id/target-id map atomically.
/// A refused mapping rolls back the entire operation, including bindings and history.
pub fn upgrade(g: &mut Graph, pairs: &[(NodeId, NodeId)]) -> Result<NameUpgradeReport, LamboError> {
    g.transaction(|g| {
        let mut report = NameUpgradeReport::default();
        // A prior explicit names-assign decision outranks an unsafe legacy
        // spelling. Only pre-existing bindings count: conflicting legacy rows
        // in this same import must still refuse instead of choosing a winner.
        let prior_names: HashSet<_> = g.names().map(|n| n.name_key.clone()).collect();
        let mut active = Vec::new();
        let mut unique = BTreeMap::new();
        for &(alias, target) in pairs {
            if let Some(prior) = unique.insert(alias.to_string(), target)
                && prior != target { return Err(invariant("one reviewed alias has two targets")); }
        }
        let aliases: HashSet<NodeId> = pairs.iter().map(|p| p.0).collect();
        for (alias, target) in unique {
            let alias = NodeId(uuid::Uuid::parse_str(&alias).expect("an id was stringified"));
            if alias == target || aliases.contains(&target) { return Err(invariant("alias repair cannot contain self references, chains or cycles")); }
            if let Some(prior) = g.retired_alias(alias) {
                if prior.target != target { return Err(invariant("a retired alias cannot be retargeted")); }
                report.already_applied += 1;
                continue;
            }
            let Some(Node::Concept(c)) = g.node(alias) else { return Err(invariant(format!("alias {alias} is not an active concept"))); };
            if c.concept_type != ConceptType::Entity { return Err(invariant(format!("alias {alias} is not an Entity; retire only reviewed name artifacts"))); }
            if !matches!(g.node(target), Some(Node::Concept(_))) { return Err(invariant(format!("target {target} is not an active concept"))); }
            if g.names().any(|n| n.node_id == alias) { return Err(invariant("alias already has immutable names; repair cannot change their identities")); }
            let edges: Vec<_> = g.incident_edges(alias).into_iter().cloned().collect();
            if edges.iter().any(|e| !matches!(e.edge_type, EdgeType::Derives | EdgeType::Semantic | EdgeType::CoOccurrence)
                && !(e.edge_type == EdgeType::Dependency && e.target == alias))
            { return Err(invariant(format!("alias {alias} has substantive relationships; resolve them explicitly before retirement"))); }
            active.push(crate::canon::alias::AliasPair { alias, ruling: target, source: crate::canon::alias::PairSource::Reviewed, line: None });
        }
        let plan = crate::canon::alias::plan(g, &active, Vec::new());
        if !plan.refusals.is_empty() || plan.redirects.iter().any(|p| p.cycle_refused > 0) {
            return Err(invariant("alias repair would create a cycle or contains a refused mapping; nothing was applied"));
        }
        // Capture history before changing any edge, including repeated citations
        // which will be deduplicated rather than counted as fresh support.
        let history: Vec<_> = active.iter().map(|p| {
            let Some(Node::Concept(c)) = g.node(p.alias) else { unreachable!() };
            RetiredAlias { session_id: g.session_id.clone(), concept: c.clone(), target: p.ruling,
                edges: g.incident_edges(p.alias).into_iter().cloned().collect(), retired_at: chrono::Utc::now() }
        }).collect();
        for id in &plan.retire { g.remove_edge(*id)?; }
        for e in &plan.add {
            g.upsert_edge(e.clone())?;
            crate::graph::suspected::settle(g, e.source, e.target)?;
        }
        for retired in history {
            let alias = retired.concept.id;
            let name = retired.concept.content.clone();
            let target = retired.target;
            g.archive_alias(retired)?;
            g.remove_node(alias)?;
            if g.declare_name(&name, target)? { report.names_migrated += 1; }
            report.aliases_retired += 1;
        }
        report.citations_moved = plan.redirects.iter().map(|p| p.redirected).sum();
        // Legacy direct synonyms are imported using canonical keys without
        // following other synonyms. Chains and missing/ambiguous targets refuse.
        let legacy: Vec<_> = g.synonyms().map(|(a,b)| (a.to_string(), b.to_string())).collect();
        for (name, target_key) in legacy {
            if prior_names.contains(&name_key(&name)?) { continue; }
            if g.synonym(&target_key).is_some() { return Err(invariant(format!("legacy synonym {name:?} chains through {target_key:?}; replace it with a direct target before migration"))); }
            let key = canonical_key(&target_key, |_| None);
            // Some historical rows contain a canonical key and others raw
            // target text. Stemming is not idempotent: consider both forms,
            // deduplicate their identities, and never choose between two.
            let mut targets: Vec<_> = g.concepts().filter(|c| c.canonical_key == key || c.canonical_key == target_key).map(|c| c.id).collect();
            targets.extend(g.retired_aliases().filter(|a| a.concept.canonical_key == key || a.concept.canonical_key == target_key).map(|a| a.target));
            targets.sort_by_key(|id| id.0);
            targets.dedup();
            let target = match targets.as_slice() {
                [one] => *one,
                [] => return Err(invariant(format!("legacy synonym {name:?} has a missing target {target_key:?}"))),
                _ => return Err(invariant(format!("legacy synonym {name:?} target {target_key:?} is ambiguous"))),
            };
            if g.declare_name(&name, target)? { report.names_migrated += 1; }
        }
        g.assert_invariants()?;
        report.mutation_epoch = g.epoch();
        Ok(report)
    })
}

#[cfg(test)]
mod tests;
