use std::collections::{BTreeMap, BTreeSet};

use super::plan::{digest, source_binding};
use super::signals::{self, Scores};
use super::{
    validate_plan, Assignment, ClassificationPlan, ClassificationRules, ClassifyError, Confidence,
    ItemKind, ItemRef, OperatorOverride, ReviewedAssignment, PLAN_VERSION,
};
use crate::types::{ConceptType, EdgeType, GraphSnapshot, NodeId};

pub(super) fn validate_source(s: &GraphSnapshot) -> Result<(), ClassifyError> {
    let mut nodes = BTreeSet::new();
    let origins: BTreeSet<_> = s.interactions.iter().map(|i| i.id.0).collect();
    if s.interactions
        .iter()
        .any(|i| i.session_id != s.session_id || !nodes.insert(i.id.0))
        || s.concepts.iter().any(|c| {
            c.session_id != s.session_id
                || !nodes.insert(c.id.0)
                || !origins.contains(&c.origin_interaction.0)
                || c.embedding
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|x| !x.is_finite()))
        })
    {
        return Err(ClassifyError::Source(
            "duplicate/foreign node, missing origin or nonfinite vector".into(),
        ));
    }
    let mut edges = BTreeSet::new();
    if s.edges.iter().any(|e| {
        e.session_id != s.session_id
            || !nodes.contains(&e.source.0)
            || !nodes.contains(&e.target.0)
            || !edges.insert(e.id.0)
            || !e.weight.is_finite()
    }) {
        return Err(ClassifyError::Source(
            "duplicate/foreign edge, missing endpoint or nonfinite weight".into(),
        ));
    }
    Ok(())
}

fn assignment(
    kind: ItemKind,
    id: NodeId,
    rules: &ClassificationRules,
    scores: &Scores,
    signals: Vec<String>,
) -> Assignment {
    let candidates = signals::ranked(scores);
    let top = signals::unique(scores);
    let confidence = match top {
        Some(p)
            if scores[p] >= 6
                && candidates
                    .get(1)
                    .is_none_or(|second| scores[p] >= 2 * scores[second]) =>
        {
            Confidence::High
        }
        Some(p)
            if scores[p] >= 6
                && candidates
                    .get(1)
                    .is_none_or(|second| scores[p] * 2 >= 3 * scores[second]) =>
        {
            Confidence::Medium
        }
        _ => Confidence::Low,
    };
    Assignment {
        kind,
        id,
        session: top.unwrap_or(&rules.fallback_session).into(),
        confidence,
        signals,
        candidates,
        pass2: false,
        copy_into: Vec::new(),
    }
}

/// Classify one immutable snapshot; nothing is flushed, leased or copied.
/// Ordering is canonical and no wall clock or generated ids enter the plan.
pub fn classify(
    snapshot: &GraphSnapshot,
    rules: &ClassificationRules,
    overrides: &[OperatorOverride],
) -> Result<ClassificationPlan, ClassifyError> {
    classify_internal(snapshot, rules, None, overrides)
}

/// Bind reviewed complete labels (such as v2) to this exact snapshot, then
/// apply separately audited operator corrections. No owner ids are built in.
pub fn classify_reviewed(
    snapshot: &GraphSnapshot,
    rules: &ClassificationRules,
    reviewed: &[ReviewedAssignment],
    overrides: &[OperatorOverride],
) -> Result<ClassificationPlan, ClassifyError> {
    classify_internal(snapshot, rules, Some(reviewed), overrides)
}

fn classify_internal(
    snapshot: &GraphSnapshot,
    rules: &ClassificationRules,
    reviewed: Option<&[ReviewedAssignment]>,
    overrides: &[OperatorOverride],
) -> Result<ClassificationPlan, ClassifyError> {
    rules.validate()?;
    validate_source(snapshot)?;
    let mut rows: BTreeMap<ItemRef, Assignment> = BTreeMap::new();
    let mut actors = BTreeMap::new();
    let mut texts = BTreeMap::new();
    let interactions: BTreeMap<_, _> = snapshot.interactions.iter().map(|i| (i.id.0, i)).collect();
    let concepts: BTreeMap<_, _> = snapshot.concepts.iter().map(|c| (c.id.0, c)).collect();
    for i in interactions.values() {
        let actor = signals::actor(rules, &i.agent_id.0);
        let (mut text, mut hints) = signals::content(rules, i.prompt_text.as_deref().unwrap_or(""));
        for c in concepts.values().filter(|c| c.origin_interaction == i.id) {
            let (s, _) = signals::content(rules, &c.content);
            // Siblings are a separate hint, capped to one vote per project.
            for p in s.keys() {
                text.entry(p.clone()).or_insert(1);
            }
        }
        let mut total = actor.clone();
        signals::merge(&mut total, &text);
        hints.extend(signals::actor_signals(rules, &i.agent_id.0));
        let key = ItemRef {
            kind: ItemKind::Interaction,
            id: i.id,
        };
        rows.insert(key, assignment(key.kind, key.id, rules, &total, hints));
        actors.insert(i.id.0, actor);
        texts.insert(i.id.0, text);
    }
    // Fixed high-confidence seeds only: no cascading/insertion-order inference.
    let mut ordered: Vec<_> = interactions.values().copied().collect();
    ordered.sort_by_key(|i| (i.about_time(), i.id.0));
    let mut neighbours = BTreeMap::new();
    for (n, i) in ordered.iter().enumerate() {
        let mut scores = Scores::new();
        for other in &ordered[n.saturating_sub(6)..(n + 7).min(ordered.len())] {
            if other.id == i.id
                || (other.about_time() - i.about_time())
                    .num_seconds()
                    .unsigned_abs()
                    > rules.neighbour_window_secs
            {
                continue;
            }
            let row = &rows[&ItemRef {
                kind: ItemKind::Interaction,
                id: other.id,
            }];
            if row.confidence == Confidence::High {
                *scores.entry(row.session.clone()).or_default() +=
                    if other.agent_id == i.agent_id { 3 } else { 1 };
            }
        }
        neighbours.insert(i.id.0, scores);
    }
    for i in interactions.values() {
        let key = ItemRef {
            kind: ItemKind::Interaction,
            id: i.id,
        };
        let row = rows.get_mut(&key).expect("interaction present");
        if row.confidence == Confidence::Low {
            row.pass2 = true;
            let n = &neighbours[&i.id.0];
            row.signals
                .push("pass2:fixed-neighbours-within-window".into());
            if let Some(p) = signals::unique(n) {
                row.candidates = signals::ranked(n);
                if signals::unique(&actors[&i.id.0]) == Some(p)
                    && signals::unique(&texts[&i.id.0]) == Some(p)
                {
                    row.session = p.into();
                    row.confidence = Confidence::Medium;
                    row.signals
                        .push("pass2:three-independent-signals-agree".into());
                }
            }
        }
    }
    for c in concepts.values() {
        let origin = &rows[&ItemRef {
            kind: ItemKind::Interaction,
            id: c.origin_interaction,
        }];
        let actor = signals::actor(rules, &c.origin_agent.0);
        let (text, mut hints) = signals::content(rules, &c.content);
        let mut total = actor.clone();
        signals::merge(&mut total, &text);
        if origin.session != rules.fallback_session {
            *total.entry(origin.session.clone()).or_default() += 3;
        }
        hints.push("origin-interaction".into());
        hints.extend(signals::actor_signals(rules, &c.origin_agent.0));
        let mut row = assignment(ItemKind::Concept, c.id, rules, &total, hints);
        // General requires three independent sources: ruling text, the
        // origin prompt, and an explicitly configured operator agent pattern.
        let i = interactions[&c.origin_interaction.0];
        let protected = rules.projects.iter().any(|p| {
            p.exclude_from_general
                && (p.session == origin.session
                    || actor.contains_key(&p.session)
                    || actors[&i.id.0].contains_key(&p.session)
                    || texts[&i.id.0].contains_key(&p.session)
                    || text.contains_key(&p.session))
        });
        if !protected
            && signals::general_ruling(rules, c)
            && signals::operator_signal(rules, i.prompt_text.as_deref().unwrap_or(""))
            && !signals::has_reference(i.prompt_text.as_deref().unwrap_or(""))
            && rules
                .general_agent_patterns
                .iter()
                .any(|p| signals::glob(p, &i.agent_id.0))
        {
            row.session = rules.general_session.clone();
            row.confidence = Confidence::High;
            row.signals
                .push("general:ruling+prompt+operator-agent-no-references".into());
        } else if row.confidence == Confidence::Low {
            row.pass2 = true;
            row.signals
                .push("pass2:content+origin+fixed-neighbours".into());
            if let Some(p) = signals::unique(&neighbours[&i.id.0])
                && signals::unique(&actor) == Some(p)
                && signals::unique(&text) == Some(p)
            {
                row.session = p.into();
                row.confidence = Confidence::Medium;
                row.signals
                    .push("pass2:three-independent-signals-agree".into());
            }
        }
        rows.insert(
            ItemRef {
                kind: row.kind,
                id: row.id,
            },
            row,
        );
    }
    // Capture protected source evidence before any imported label or override
    // can erase it. Both recorded actors and text evidence matter.
    let protected_interactions: BTreeSet<_> = interactions
        .values()
        .filter(|i| {
            rules.projects.iter().any(|p| {
                p.exclude_from_general
                    && (actors[&i.id.0].contains_key(&p.session)
                        || texts[&i.id.0].contains_key(&p.session))
            })
        })
        .map(|i| i.id.0)
        .collect();
    let protected_concepts: BTreeSet<_> = concepts
        .values()
        .filter(|c| {
            protected_interactions.contains(&c.origin_interaction.0)
                || rules.projects.iter().any(|p| {
                    p.exclude_from_general
                        && (signals::actor(rules, &c.origin_agent.0).contains_key(&p.session)
                            || signals::content(rules, &c.content)
                                .0
                                .contains_key(&p.session))
                })
        })
        .map(|c| c.id.0)
        .collect();
    let mut reviewed_sha = None;
    if let Some(reviewed) = reviewed {
        let expected: BTreeSet<_> = rows.keys().copied().collect();
        let actual: BTreeSet<_> = reviewed
            .iter()
            .map(|r| ItemRef {
                kind: r.kind,
                id: r.id,
            })
            .collect();
        if expected != actual || actual.len() != reviewed.len() {
            return Err(ClassifyError::Rules(
                "reviewed labels must cover every source interaction/concept exactly once".into(),
            ));
        }
        let allowed = rules.sessions();
        let mut sorted = reviewed.to_vec();
        sorted.sort_by_key(|r| (r.kind, r.id.0));
        for r in &sorted {
            if !allowed.contains(&r.project)
                || r.copy_into
                    .iter()
                    .any(|s| !allowed.contains(s) || s == &rules.general_session)
                || (!r.copy_into.is_empty()
                    && (!r.copy_into.contains(&r.project)
                        || r.kind != ItemKind::Concept
                        || !concepts.get(&r.id.0).is_some_and(|c| {
                            matches!(c.concept_type, ConceptType::Resource | ConceptType::Entity)
                        })))
            {
                return Err(ClassifyError::Rules(
                    "reviewed label has invalid session or non-identity hub placement".into(),
                ));
            }
            if r.project == rules.general_session
                && r.kind == ItemKind::Concept
                && !concepts.get(&r.id.0).is_some_and(|c| {
                    matches!(c.concept_type, ConceptType::Constraint | ConceptType::Logic)
                })
            {
                return Err(ClassifyError::Rules(
                    "reviewed general label must be a ruling".into(),
                ));
            }
            if r.project == rules.general_session
                && r.kind == ItemKind::Concept
                && !signals::general_ruling(rules, concepts[&r.id.0])
                && !overrides.iter().any(|o| {
                    o.kind == r.kind
                        && o.id == r.id
                        && o.session == rules.general_session
                        && !o.reason.trim().is_empty()
                })
            {
                return Err(ClassifyError::Rules("reviewed general ruling violates conservative guards; supply an explicitly audited operator correction".into()));
            }
            let key = ItemRef {
                kind: r.kind,
                id: r.id,
            };
            let row = rows.get_mut(&key).expect("reviewed key checked");
            row.session = r.project.clone();
            row.confidence = r.confidence;
            row.pass2 = r.pass2;
            row.copy_into = r.copy_into.clone();
            row.signals.extend(r.signals.clone());
            row.signals.push("reviewed-assignment".into());
        }
        // Reviewed labels do not authorize exporting protected provenance.
        for r in &sorted {
            if r.project != rules.general_session {
                continue;
            }
            let protected = match r.kind {
                ItemKind::Concept => protected_concepts.contains(&r.id.0),
                ItemKind::Interaction => protected_interactions.contains(&r.id.0),
            };
            if protected {
                return Err(ClassifyError::Rules(
                    "reviewed general label originated in a protected project".into(),
                ));
            }
            if r.kind == ItemKind::Interaction {
                let children: Vec<_> = concepts
                    .values()
                    .filter(|c| c.origin_interaction == r.id)
                    .collect();
                if children.is_empty()
                    || children.iter().any(|c| {
                        rows[&ItemRef {
                            kind: ItemKind::Concept,
                            id: c.id,
                        }]
                            .session
                            != rules.general_session
                    })
                {
                    return Err(ClassifyError::Rules(
                        "reviewed general interaction requires all origin concepts in general"
                            .into(),
                    ));
                }
            }
        }
        reviewed_sha = Some(digest(&sorted)?);
    }
    let sessions = rules.sessions();
    let mut used = BTreeSet::new();
    let mut ordered_overrides = overrides.to_vec();
    ordered_overrides.sort_by_key(|o| (o.kind, o.id.0));
    for o in &ordered_overrides {
        let key = ItemRef {
            kind: o.kind,
            id: o.id,
        };
        if !used.insert(key)
            || !sessions.contains(&o.session)
            || o.reason.trim().is_empty()
            || o.reason.len() > 4096
            || o.copy_into.iter().any(|s| !sessions.contains(s))
        {
            return Err(ClassifyError::Rules(
                "duplicate override, invalid target/placements or empty/oversized reason".into(),
            ));
        }
        let protected_origin = protected_concepts.contains(&o.id.0);
        let row = rows.get_mut(&key).ok_or_else(|| {
            ClassifyError::Rules("override refers to a missing or foreign item".into())
        })?;
        if o.from.as_ref().is_some_and(|s| s != &row.session) {
            return Err(ClassifyError::Rules(
                "override expected assignment does not match".into(),
            ));
        }
        if o.session == rules.general_session {
            let Some(c) = concepts
                .get(&o.id.0)
                .filter(|_| o.kind == ItemKind::Concept)
            else {
                return Err(ClassifyError::Rules(
                    "general override must name a ruling concept".into(),
                ));
            };
            if protected_origin
                || !matches!(c.concept_type, ConceptType::Logic | ConceptType::Constraint)
                || rules.projects.iter().any(|p| {
                    p.exclude_from_general
                        && (p.session == row.session
                            || signals::actor(rules, &c.origin_agent.0).contains_key(&p.session))
                })
            {
                return Err(ClassifyError::Rules(
                    "general override refuses identity/non-ruling or protected project record"
                        .into(),
                ));
            }
        }
        if !o.copy_into.is_empty()
            && (!concepts.get(&o.id.0).is_some_and(|c| {
                matches!(c.concept_type, ConceptType::Resource | ConceptType::Entity)
            }) || !o.copy_into.contains(&o.session)
                || o.copy_into.contains(&rules.general_session))
        {
            return Err(ClassifyError::Rules(
                "hub override must name an identity and include primary, never general".into(),
            ));
        }
        row.session = o.session.clone();
        row.confidence = Confidence::High;
        row.signals.push("operator-override".into());
        row.copy_into = o.copy_into.clone();
    }
    // General origins are provenance placements for D2, not copied here.
    // Assign an interaction to general only when ALL its concepts are there.
    for i in interactions.values() {
        let children: Vec<_> = concepts
            .values()
            .filter(|c| c.origin_interaction == i.id)
            .collect();
        let key = ItemRef {
            kind: ItemKind::Interaction,
            id: i.id,
        };
        if reviewed.is_none()
            && !used.contains(&key)
            && !children.is_empty()
            && children.iter().all(|c| {
                rows[&ItemRef {
                    kind: ItemKind::Concept,
                    id: c.id,
                }]
                    .session
                    == rules.general_session
            })
        {
            let row = rows.get_mut(&key).expect("interaction present");
            row.session = rules.general_session.clone();
            row.confidence = Confidence::Medium;
            row.signals.push("all-origin-concepts-general".into());
        }
    }
    let seeds = rows.clone();
    for c in concepts
        .values()
        .filter(|c| matches!(c.concept_type, ConceptType::Resource | ConceptType::Entity))
    {
        let key = ItemRef {
            kind: ItemKind::Concept,
            id: c.id,
        };
        let incident: BTreeSet<_> = snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type != EdgeType::Temporal && (e.source == c.id || e.target == c.id))
            .map(|e| {
                if e.source == c.id {
                    e.target.0
                } else {
                    e.source.0
                }
            })
            .collect();
        rows.get_mut(&key)
            .expect("identity present")
            .signals
            .extend(
                incident
                    .into_iter()
                    .map(|other| format!("identity-incident:{other}")),
            );
        if reviewed.is_some() || used.contains(&key) && !rows[&key].copy_into.is_empty() {
            continue;
        }
        let mut placements = BTreeSet::from([rows[&key].session.clone()]);
        for e in snapshot
            .edges
            .iter()
            .filter(|e| e.edge_type != EdgeType::Temporal && (e.source == c.id || e.target == c.id))
        {
            let other = if e.source == c.id { e.target } else { e.source };
            let kind = if interactions.contains_key(&other.0) {
                ItemKind::Interaction
            } else {
                ItemKind::Concept
            };
            let target = &seeds[&ItemRef { kind, id: other }].session;
            if target != &rules.general_session {
                placements.insert(target.clone());
            }
        }
        if placements.len() > 1 {
            let row = rows.get_mut(&key).expect("identity present");
            row.copy_into = placements.into_iter().collect();
            row.confidence = Confidence::Hub;
            row.signals
                .push("shared-identity-hub:incident-project-placements".into());
        }
    }
    for row in rows.values_mut() {
        row.signals.sort();
        row.signals.dedup();
        row.candidates.sort();
        row.candidates.dedup();
        row.copy_into.sort();
        row.copy_into.dedup();
    }
    let assignments: Vec<_> = rows.into_values().collect();
    let low_confidence = assignments
        .iter()
        .filter(|a| a.confidence == Confidence::Low)
        .map(|a| ItemRef {
            kind: a.kind,
            id: a.id,
        })
        .collect();
    let mut plan = ClassificationPlan {
        version: PLAN_VERSION,
        source: source_binding(snapshot)?,
        rules: rules.clone(),
        rules_sha256: digest(rules)?,
        assignments,
        low_confidence,
        overrides: ordered_overrides,
        reviewed_assignments_sha256: reviewed_sha,
        plan_sha256: String::new(),
    };
    plan.seal()?;
    validate_plan(&plan, snapshot)?;
    Ok(plan)
}
