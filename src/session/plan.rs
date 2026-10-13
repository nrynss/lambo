use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{GraphSnapshot, NodeId};

/// Schema/algorithm version. D2 must reject versions it does not implement.
pub const PLAN_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("classification rules: {0}")]
    Rules(String),
    #[error("classification source: {0}")]
    Source(String),
    #[error("classification plan: {0}")]
    Plan(String),
}

/// Classification hints; these select evidence, never authorize a live write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProjectRule {
    pub session: String,
    #[serde(default)]
    pub folders: Vec<String>,
    /// Anchored, case-sensitive glob patterns: `*` and `?` only.
    #[serde(default)]
    pub agent_id_patterns: Vec<String>,
    /// Case-insensitive literal phrases, with word boundaries.
    #[serde(default)]
    pub content_keywords: Vec<String>,
    /// Records originating in this project can never be shared into general.
    #[serde(default)]
    pub exclude_from_general: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ClassificationRules {
    pub projects: Vec<ProjectRule>,
    pub general_session: String,
    pub fallback_session: String,
    pub operator_wide_phrases: Vec<String>,
    pub general_agent_patterns: Vec<String>,
    /// Event-time neighbours within this bound; at most six on either side.
    pub neighbour_window_secs: u64,
}

impl Default for ClassificationRules {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            general_session: "general".into(),
            fallback_session: "general-usage".into(),
            operator_wide_phrases: vec![
                "all projects".into(),
                "every repository".into(),
                "operator-wide".into(),
                "agent protocol".into(),
            ],
            general_agent_patterns: Vec::new(),
            neighbour_window_secs: 7200,
        }
    }
}

fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && s != "."
        && s != ".."
}

impl ClassificationRules {
    pub fn sessions(&self) -> BTreeSet<String> {
        self.projects
            .iter()
            .map(|p| p.session.clone())
            .chain([self.general_session.clone(), self.fallback_session.clone()])
            .collect()
    }

    pub fn validate(&self) -> Result<(), ClassifyError> {
        if !valid_session(&self.general_session)
            || !valid_session(&self.fallback_session)
            || self.general_session == self.fallback_session
            || self.projects.len() > 128
            || self.neighbour_window_secs > 86_400
        {
            return Err(ClassifyError::Rules(
                "invalid/reserved session, project count or neighbour window".into(),
            ));
        }
        let mut seen = BTreeSet::from([
            self.general_session.as_str(),
            self.fallback_session.as_str(),
        ]);
        for p in &self.projects {
            if !valid_session(&p.session) || !seen.insert(&p.session) {
                return Err(ClassifyError::Rules(
                    "invalid or duplicate project session".into(),
                ));
            }
            for values in [&p.folders, &p.agent_id_patterns, &p.content_keywords] {
                check_patterns(values)?;
            }
            for folder in &p.folders {
                if !folder.starts_with('/')
                    || folder.split('/').any(|c| matches!(c, "." | ".."))
                    || folder.contains(['*', '?'])
                {
                    return Err(ClassifyError::Rules(
                        "project folders must be absolute literal paths without dot components"
                            .into(),
                    ));
                }
            }
        }
        check_patterns(&self.operator_wide_phrases)?;
        check_patterns(&self.general_agent_patterns)
    }
}

fn check_patterns(values: &[String]) -> Result<(), ClassifyError> {
    let mut seen = BTreeSet::new();
    if values.len() > 128
        || values.iter().any(|s| {
            s.trim() != s
                || s.is_empty()
                || s.len() > 4096
                || s.chars().any(char::is_control)
                || !seen.insert(s)
        })
    {
        return Err(ClassifyError::Rules(
            "empty, oversized, duplicate or control-bearing hint".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Interaction,
    Concept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRef {
    pub kind: ItemKind,
    pub id: NodeId,
}

impl Ord for ItemRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.kind, self.id.0).cmp(&(other.kind, other.id.0))
    }
}
impl PartialOrd for ItemRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
    Hub,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub kind: ItemKind,
    pub id: NodeId,
    pub session: String,
    pub confidence: Confidence,
    pub signals: Vec<String>,
    pub candidates: Vec<String>,
    pub pass2: bool,
    /// Identity placements, including the primary; D1 does not copy anything.
    pub copy_into: Vec<String>,
}

/// Explicit operator corrections, applied after automated classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorOverride {
    /// Explicit historical shared-hub assertion, only with a complete reviewed import.
    #[serde(default)]
    pub shared_hub_assertion: bool,
    pub kind: ItemKind,
    pub id: NodeId,
    #[serde(alias = "to")]
    pub session: String,
    pub reason: String,
    /// Optional expected current assignment; prevents stale corrections.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub copy_into: Vec<String>,
}

impl OperatorOverride {
    pub(super) fn permits_reviewed_replication(
        &self,
        kind: ItemKind,
        id: NodeId,
        session: &str,
        placements: &[String],
    ) -> bool {
        self.shared_hub_assertion
            && self.kind == kind
            && self.id == id
            && self.session == session
            && !self.reason.trim().is_empty()
            && !placements.is_empty()
            && self.copy_into.iter().collect::<BTreeSet<_>>()
                == placements.iter().collect::<BTreeSet<_>>()
    }
}

/// Explicitly reviewed complete labels, compatible with the v2 JSONL shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedAssignment {
    pub kind: ItemKind,
    pub id: NodeId,
    #[serde(alias = "session")]
    pub project: String,
    pub confidence: Confidence,
    pub signals: Vec<String>,
    pub pass2: bool,
    #[serde(default)]
    pub copy_into: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub session: String,
    pub sha256: String,
    pub interactions: usize,
    pub concepts: usize,
    pub edges: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassificationPlan {
    pub version: u32,
    pub source: SourceBinding,
    pub rules: ClassificationRules,
    pub rules_sha256: String,
    pub assignments: Vec<Assignment>,
    pub low_confidence: Vec<ItemRef>,
    pub overrides: Vec<OperatorOverride>,
    pub reviewed_assignments_sha256: Option<String>,
    /// Corruption detection, not an authorization/signature mechanism.
    pub plan_sha256: String,
}

/// Canonical JSON: object keys sorted recursively; meaningful array order
/// retained. Durable row collections are sorted separately before hashing.
fn canonical(v: &serde_json::Value, out: &mut Vec<u8>) -> Result<(), ClassifyError> {
    match v {
        serde_json::Value::Object(map) => {
            out.push(b'{');
            for (i, (key, value)) in map
                .iter()
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .enumerate()
            {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(
                    serde_json::to_vec(key).map_err(|e| ClassifyError::Source(e.to_string()))?,
                );
                out.push(b':');
                canonical(value, out)?;
            }
            out.push(b'}');
        }
        serde_json::Value::Array(values) => {
            out.push(b'[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(value, out)?;
            }
            out.push(b']');
        }
        _ => out.extend(serde_json::to_vec(v).map_err(|e| ClassifyError::Source(e.to_string()))?),
    }
    Ok(())
}

pub(super) fn digest<T: Serialize>(value: &T) -> Result<String, ClassifyError> {
    let mut bytes = Vec::new();
    canonical(
        &serde_json::to_value(value).map_err(|e| ClassifyError::Source(e.to_string()))?,
        &mut bytes,
    )?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub(super) fn source_binding(snapshot: &GraphSnapshot) -> Result<SourceBinding, ClassifyError> {
    let mut value =
        serde_json::to_value(snapshot).map_err(|e| ClassifyError::Source(e.to_string()))?;
    let obj = value.as_object_mut().expect("snapshot is a struct");
    // Reservations are live holder state; expiry must not stale a durable plan.
    obj.remove("reservations");
    for field in ["interactions", "concepts", "edges", "synonyms"] {
        if let Some(rows) = obj.get_mut(field).and_then(serde_json::Value::as_array_mut) {
            rows.sort_by_cached_key(|r| {
                if let Some(id) = r.get("id").and_then(serde_json::Value::as_str) {
                    id.to_owned()
                } else {
                    // Synonyms use their complete canonical natural key.
                    let mut bytes = Vec::new();
                    canonical(r, &mut bytes).expect("JSON value serializes");
                    String::from_utf8(bytes).expect("canonical JSON is UTF-8")
                }
            });
        }
    }
    Ok(SourceBinding {
        session: snapshot.session_id.0.clone(),
        sha256: digest(&value)?,
        interactions: snapshot.interactions.len(),
        concepts: snapshot.concepts.len(),
        edges: snapshot.edges.len(),
    })
}

impl ClassificationPlan {
    pub(super) fn seal(&mut self) -> Result<(), ClassifyError> {
        self.plan_sha256.clear();
        self.plan_sha256 = digest(self)?;
        Ok(())
    }
}

/// Refuse stale, unsupported or edited plans. D2 can use this before its
/// first write; D1 itself validates every generated plan through the same API.
pub fn validate_plan(
    plan: &ClassificationPlan,
    snapshot: &GraphSnapshot,
) -> Result<(), ClassifyError> {
    plan.rules.validate()?;
    super::classify::validate_source(snapshot)?;
    if plan.version != PLAN_VERSION
        || plan.source != source_binding(snapshot)?
        || plan.rules_sha256 != digest(&plan.rules)?
    {
        return Err(ClassifyError::Plan(
            "unsupported version or stale source/rules fingerprint".into(),
        ));
    }
    let mut sealed = plan.clone();
    sealed.seal()?;
    if sealed.plan_sha256 != plan.plan_sha256 {
        return Err(ClassifyError::Plan(
            "plan fingerprint mismatch; apply corrections using overrides and reclassify".into(),
        ));
    }
    let expected: BTreeSet<_> = snapshot
        .interactions
        .iter()
        .map(|i| ItemRef {
            kind: ItemKind::Interaction,
            id: i.id,
        })
        .chain(snapshot.concepts.iter().map(|c| ItemRef {
            kind: ItemKind::Concept,
            id: c.id,
        }))
        .collect();
    let actual: BTreeSet<_> = plan
        .assignments
        .iter()
        .map(|a| ItemRef {
            kind: a.kind,
            id: a.id,
        })
        .collect();
    if actual != expected || actual.len() != plan.assignments.len() {
        return Err(ClassifyError::Plan(
            "duplicate, missing or foreign assignment".into(),
        ));
    }
    let sessions = plan.rules.sessions();
    for a in &plan.assignments {
        if !sessions.contains(&a.session)
            || a.candidates
                .iter()
                .chain(&a.copy_into)
                .any(|s| !sessions.contains(s))
            || (!a.copy_into.is_empty()
                && (a.kind != ItemKind::Concept || !a.copy_into.contains(&a.session)))
        {
            return Err(ClassifyError::Plan(
                "invalid target, candidate or hub placement".into(),
            ));
        }
    }
    if plan
        .reviewed_assignments_sha256
        .as_ref()
        .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(ClassifyError::Plan(
            "invalid reviewed import fingerprint".into(),
        ));
    }
    let mut audited = BTreeSet::new();
    for o in &plan.overrides {
        let key = ItemRef {
            kind: o.kind,
            id: o.id,
        };
        if !expected.contains(&key)
            || !audited.insert(key)
            || o.reason.trim().is_empty()
            || o.reason.len() > 4096
            || !sessions.contains(&o.session)
            || o.from.as_ref().is_some_and(|s| !sessions.contains(s))
            || o.copy_into
                .iter()
                .any(|s| !sessions.contains(s) || s == &plan.rules.general_session)
            || (!o.copy_into.is_empty()
                && (o.kind != ItemKind::Concept || !o.copy_into.contains(&o.session)))
            || (o.shared_hub_assertion
                && (plan.reviewed_assignments_sha256.is_none()
                    || !plan.assignments.iter().any(|a| {
                        o.permits_reviewed_replication(a.kind, a.id, &a.session, &a.copy_into)
                    })))
        {
            return Err(ClassifyError::Plan(
                "invalid, duplicate or foreign operator audit".into(),
            ));
        }
    }
    let (protected_interactions, protected_concepts) =
        super::classify::protected_source_items(snapshot, &plan.rules);
    for a in plan
        .assignments
        .iter()
        .filter(|a| a.session == plan.rules.general_session)
    {
        match a.kind {
            ItemKind::Interaction if protected_interactions.contains(&a.id.0) => {
                return Err(ClassifyError::Plan(
                    "general interaction has protected source provenance".into(),
                ))
            }
            ItemKind::Concept => {
                let c = snapshot
                    .concepts
                    .iter()
                    .find(|c| c.id == a.id)
                    .expect("assignment source checked");
                let audited_general = plan.overrides.iter().any(|o| {
                    o.kind == a.kind && o.id == a.id && o.session == plan.rules.general_session
                });
                let auto_origin = snapshot
                    .interactions
                    .iter()
                    .find(|i| i.id == c.origin_interaction)
                    .is_some_and(|i| {
                        super::signals::operator_signal(
                            &plan.rules,
                            i.prompt_text.as_deref().unwrap_or(""),
                        ) && !super::signals::has_reference(i.prompt_text.as_deref().unwrap_or(""))
                            && plan
                                .rules
                                .general_agent_patterns
                                .iter()
                                .any(|p| super::signals::glob(p, &i.agent_id.0))
                    });
                // A mention of a project excluded from general, in the origin
                // prompt or a sibling concept, is soft evidence: only an
                // explicit audit may waive it. This matches the classifier's
                // automatic guard; other projects' mentions do not block general.
                let soft_origin_mention = snapshot
                    .interactions
                    .iter()
                    .filter(|i| i.id == c.origin_interaction)
                    .map(|i| i.prompt_text.as_deref().unwrap_or(""))
                    .chain(
                        snapshot
                            .concepts
                            .iter()
                            .filter(|s| s.origin_interaction == c.origin_interaction)
                            .map(|s| s.content.as_str()),
                    )
                    .any(|text| {
                        let mentioned = super::signals::content(&plan.rules, text).0;
                        plan.rules
                            .projects
                            .iter()
                            .any(|p| p.exclude_from_general && mentioned.contains_key(&p.session))
                    });
                if (plan.reviewed_assignments_sha256.is_none() && !auto_origin && !audited_general)
                    || (soft_origin_mention && !audited_general)
                    || protected_concepts.contains(&a.id.0)
                    || !matches!(
                        c.concept_type,
                        crate::types::ConceptType::Logic | crate::types::ConceptType::Constraint
                    )
                    || (!super::signals::general_ruling(&plan.rules, c)
                        && !plan.overrides.iter().any(|o| {
                            o.kind == a.kind
                                && o.id == a.id
                                && o.session == plan.rules.general_session
                        }))
                {
                    return Err(ClassifyError::Plan("general concept must be an unprotected ruling with conservative evidence or explicit audit".into()));
                }
            }
            _ => (),
        }
    }
    for a in plan.assignments.iter().filter(|a| !a.copy_into.is_empty()) {
        let concept = snapshot
            .concepts
            .iter()
            .find(|c| c.id == a.id)
            .ok_or_else(|| ClassifyError::Plan("hub must name a concept".into()))?;
        let identity = matches!(
            concept.concept_type,
            crate::types::ConceptType::Resource | crate::types::ConceptType::Entity
        );
        if a.session == plan.rules.general_session
            || a.copy_into.contains(&plan.rules.general_session)
            || (!identity
                && (plan.reviewed_assignments_sha256.is_none()
                    || !plan.overrides.iter().any(|o| {
                        o.permits_reviewed_replication(a.kind, a.id, &a.session, &a.copy_into)
                    })))
        {
            return Err(ClassifyError::Plan("nonidentity replication requires an audited reviewed shared-hub assertion, never general".into()));
        }
    }
    // General interaction provenance must remain general after corrections.
    for a in plan
        .assignments
        .iter()
        .filter(|a| a.kind == ItemKind::Interaction && a.session == plan.rules.general_session)
    {
        let children: Vec<_> = snapshot
            .concepts
            .iter()
            .filter(|c| c.origin_interaction == a.id)
            .collect();
        if children.is_empty()
            || children.iter().any(|c| {
                !plan.assignments.iter().any(|r| {
                    r.kind == ItemKind::Concept
                        && r.id == c.id
                        && r.session == plan.rules.general_session
                })
            })
        {
            return Err(ClassifyError::Plan(
                "general interaction requires all origin concepts in general".into(),
            ));
        }
    }
    let low: Vec<_> = plan
        .assignments
        .iter()
        .filter(|a| a.confidence == Confidence::Low)
        .map(|a| ItemRef {
            kind: a.kind,
            id: a.id,
        })
        .collect();
    if low != plan.low_confidence {
        return Err(ClassifyError::Plan(
            "low-confidence inventory does not match assignments".into(),
        ));
    }
    Ok(())
}
