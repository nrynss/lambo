//! #148: re-key stored concepts after identifier tokens stopped being stemmed.
//!
//! Builds before #148 stemmed every token, so a stored concept whose content
//! holds an identifier token (`v2`, `2122de39e47e`, `src/main.rs`) carries a
//! key the current [`canonical_key`] no longer derives, and a new write of the
//! same text would mint a second concept. `lambo provision` calls
//! [`crate::store::GraphStore::rekey_canonical_keys`], which reads every
//! concept's `(id, session, content, key, type)` and the synonyms, plans here,
//! and writes the plan in one transaction.
//!
//! The plan only touches a row whose stored key is exactly what the old rule
//! derives from its content (through the session's synonyms). Anything else
//! (a repository-scoped Resource key, a key a current build wrote) is left
//! alone, so a rerun finds nothing to do. Already-merged concepts stay merged:
//! the surviving concept keeps the content its first write gave it.
//!
//! A new key that another concept in the session already holds (possible
//! only when a current build wrote that concept before the re-key ran) would
//! break `UNIQUE (session_id, canonical_key)`; that row keeps its old key and
//! is reported, never merged.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::graph::canonical::{canonical_key, legacy_canonical_key};

/// One stored concept, as the re-key reads it. `id` is the adapter's id text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRow {
    pub id: String,
    pub session_id: String,
    pub content: String,
    pub canonical_key: String,
    pub observation: bool,
}

/// One planned key change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChange {
    pub id: String,
    pub old_key: String,
    pub new_key: String,
}

/// What the re-key will do (or did).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RekeyPlan {
    /// Rows to re-key, sorted by id.
    pub changes: Vec<KeyChange>,
    /// Rows that keep their old key because the new one is taken, by id.
    pub collisions: Vec<String>,
}

/// Plan the re-key. `synonyms` maps `(session_id, source_key)` to the
/// synonym's target text, as the `synonyms` table stores it.
pub fn plan_rekey(rows: &[KeyRow], synonyms: &HashMap<(String, String), String>) -> RekeyPlan {
    let mut candidates: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        // The write path's raw lookup (trimmed, exact) before normalizing;
        // trimming the target again changes no token.
        let raw = r.content.trim();
        let effective = synonyms
            .get(&(r.session_id.clone(), raw.to_string()))
            .map_or(raw, String::as_str);
        if legacy_canonical_key(effective, |_| None) != r.canonical_key {
            continue;
        }
        let new_key = canonical_key(effective, |_| None);
        if new_key != r.canonical_key {
            candidates.insert(r.id.clone(), (i, new_key));
        }
    }
    // Fixpoint: a candidate whose new key is held by a row that keeps its key
    // is refused, and then its own old key stays held too.
    let mut refused: HashSet<String> = HashSet::new();
    loop {
        let mut held: HashSet<(&str, &str)> = HashSet::new();
        for r in rows.iter().filter(|r| !r.observation) {
            if !candidates.contains_key(&r.id) || refused.contains(&r.id) {
                held.insert((&r.session_id, &r.canonical_key));
            }
        }
        let mut changed = false;
        for (id, (i, new_key)) in &candidates {
            let r = &rows[*i];
            if r.observation || refused.contains(id) {
                continue;
            }
            if !held.insert((&r.session_id, new_key)) {
                refused.insert(id.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut plan = RekeyPlan::default();
    for (id, (i, new_key)) in candidates {
        if refused.contains(&id) {
            plan.collisions.push(id);
        } else {
            plan.changes.push(KeyChange {
                id,
                old_key: rows[i].canonical_key.clone(),
                new_key,
            });
        }
    }
    plan
}

/// The placeholder a changed row holds between the two update passes, so no
/// intermediate state can trip the unique index (two rows trading keys). The
/// normaliser splits on `-`, so no derived key contains `lambo-rekey`; and
/// it is plain text, which Postgres accepts (it refuses NUL).
pub fn placeholder_key(id: &str) -> String {
    format!("~lambo-rekey~:{id}")
}

/// The first live lease among `(session, holder)` rows whose lease has not
/// expired, skipping the store's own reserved rows: an erased session's
/// tombstone never expires and a released row names no process, so neither
/// is a running holder.
pub fn first_live_holder(rows: &[(String, String)]) -> Option<&(String, String)> {
    rows.iter()
        .find(|(_, holder)| !crate::store::lease::is_reserved_holder(holder))
}

/// The refusal while a session is held: a live holder's graph still carries
/// the old keys and would flush them back (or mint a duplicate).
pub fn live_lease_err(session: &str, holder: &str, pending: usize) -> crate::types::StoreError {
    crate::types::StoreError::Capability(format!(
        "cannot re-key {pending} concept(s) for unstemmed identifiers (#148) while session \
         {session:?} is held by {holder:?}: a running holder keeps the old keys in memory. \
         Stop every holder on this store, then rerun `lambo provision`"
    ))
}

/// The provision line for a plan that was applied.
pub fn report(plan: &RekeyPlan) -> String {
    let mut out = format!(
        "re-keyed {} concept(s) for unstemmed identifiers (#148)",
        plan.changes.len()
    );
    if !plan.collisions.is_empty() {
        out.push_str(&format!(
            "; {} kept their old key because a concept written since already holds the new \
             one: {}",
            plan.collisions.len(),
            plan.collisions.join(", ")
        ));
    }
    out
}

/// #148 testkit, any durable adapter: plant `sid` with concepts keyed by the
/// pre-#148 rule (as an old build flushed them), and after the caller has
/// released its lease, re-key and check every key is the current rule's and a
/// rerun is a no-op. Other sessions in a shared live database may also be
/// re-keyed; the checks look only at `sid`.
#[cfg(all(test, any(feature = "store-sqlite", feature = "store-postgres")))]
pub(crate) mod testkit {
    use chrono::Utc;

    use crate::graph::canonical::{canonical_key, legacy_canonical_key};
    use crate::store::GraphStore;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Interaction, Mutation, MutationBatch,
        Node, NodeId, SessionId,
    };

    pub(crate) const CONTENTS: [&str; 3] = ["2122de39e47", "release 2122de39e47e", "update users"];

    pub(crate) async fn plant_legacy(store: &dyn GraphStore, sid: &SessionId, token: Option<u64>) {
        let origin = NodeId::new();
        let mut mutations = vec![Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                event_time: None,
                id: origin,
                session_id: sid.clone(),
                agent_id: AgentId::new("rekey-test"),
                prompt_text: Some("q".into()),
                previous_id: None,
                created_at: Utc::now(),
            }),
        }];
        for content in CONTENTS {
            mutations.push(Mutation::UpsertNode {
                node: Node::Concept(Concept {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    content: content.into(),
                    canonical_key: legacy_canonical_key(content, |_| None),
                    concept_type: ConceptType::Entity,
                    origin_interaction: origin,
                    origin_agent: AgentId::new("rekey-test"),
                    created_at: Utc::now(),
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: None,
                    human_confirmed: 0,
                    chunk_group_id: None,
                    embedding_source: None,
                }),
            });
        }
        store
            .flush(
                &MutationBatch {
                    mutations,
                    ..Default::default()
                },
                token,
            )
            .await
            .expect("flush the legacy-keyed session");
    }

    pub(crate) async fn check_rekeyed(store: &dyn GraphStore, sid: &SessionId) {
        let keys = |snap: crate::types::GraphSnapshot| {
            let mut k: Vec<(String, String)> = snap
                .concepts
                .into_iter()
                .map(|c| (c.content, c.canonical_key))
                .collect();
            k.sort();
            k
        };
        let before = keys(store.load_session(sid).await.expect("load"));
        assert_eq!(
            before[1].1, "2122de39e47 releas",
            "planted under the old rule"
        );
        let plan = store.rekey_canonical_keys().await.expect("rekey");
        assert!(
            plan.changes
                .iter()
                .any(|c| c.new_key == "2122de39e47e releas"),
            "{plan:?}"
        );
        let after = keys(store.load_session(sid).await.expect("load"));
        for (content, key) in &after {
            assert_eq!(key, &canonical_key(content, |_| None), "{content}");
        }
        let again = store.rekey_canonical_keys().await.expect("rerun");
        assert!(
            again.changes.is_empty(),
            "a rerun re-keys nothing: {again:?}"
        );
        assert_eq!(keys(store.load_session(sid).await.expect("load")), after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, content: &str, key: &str) -> KeyRow {
        KeyRow {
            id: id.into(),
            session_id: "s".into(),
            content: content.into(),
            canonical_key: key.into(),
            observation: false,
        }
    }

    fn none() -> HashMap<(String, String), String> {
        HashMap::new()
    }

    #[test]
    fn identifier_rows_written_by_the_old_rule_are_rekeyed() {
        let rows = [
            row("a", "2122de39e47e", "2122de39e47"),
            row("b", "update users", "updat user"),
            row(
                "c",
                "deploy v2 schemas",
                &legacy_canonical_key("deploy v2 schemas", |_| None),
            ),
        ];
        let plan = plan_rekey(&rows, &none());
        assert_eq!(
            plan.changes,
            vec![KeyChange {
                id: "a".into(),
                old_key: "2122de39e47".into(),
                new_key: "2122de39e47e".into(),
            }]
        );
        // `v2` was never changed by the stemmer, so `c` keeps its key.
        assert!(plan.collisions.is_empty());
    }

    #[test]
    fn a_rerun_and_foreign_keys_are_left_alone() {
        let rows = [
            // Already re-keyed.
            row("a", "2122de39e47e", "2122de39e47e"),
            // A repository-scoped Resource key: not the old rule's output.
            row(
                "b",
                "src/release.rs",
                "RepoResource:v1:[\"r\",\"src/release.rs\"]",
            ),
        ];
        assert_eq!(plan_rekey(&rows, &none()), RekeyPlan::default());
    }

    #[test]
    fn synonyms_are_followed_like_the_write_path() {
        let mut syn = none();
        syn.insert(
            ("s".into(), "release".into()),
            "release 2122de39e47e".into(),
        );
        let old = legacy_canonical_key("release", |r| {
            (r == "release").then_some("release 2122de39e47e")
        });
        let rows = [row("a", "release", &old)];
        let plan = plan_rekey(&rows, &syn);
        assert_eq!(plan.changes[0].new_key, "2122de39e47e releas");
    }

    #[test]
    fn a_taken_new_key_is_refused_and_reported_not_merged() {
        let rows = [
            row(
                "a",
                "build 2122de39e47e",
                &legacy_canonical_key("build 2122de39e47e", |_| None),
            ),
            // Written by a current build before provision ran.
            row("z", "build 2122de39e47e", "2122de39e47e build"),
        ];
        let plan = plan_rekey(&rows, &none());
        assert!(plan.changes.is_empty());
        assert_eq!(plan.collisions, vec!["a".to_string()]);
        // Another session is independent.
        let mut other = rows.clone();
        other[1].session_id = "t".into();
        assert_eq!(plan_rekey(&other, &none()).changes.len(), 1);
        // Observations are exempt from uniqueness, so they are re-keyed.
        let mut obs = rows.clone();
        obs[0].observation = true;
        assert_eq!(plan_rekey(&obs, &none()).changes.len(), 1);
    }
}
