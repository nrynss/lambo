//! Shared scaffolding for the property tests that restate the Lean theorems
//! in `formal/` against the real code (#291).
//!
//! Each property test cites the theorem it states by name and file, so the
//! Lean statement and the Rust one can be compared by eye. The tests live
//! next to the code they exercise; this module holds only what they share:
//! the run configuration and a few graph builders.

use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use proptest::test_runner::Config;
use uuid::Uuid;

use crate::graph::resource::RESOURCE_KEY_PREFIX;
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Interaction, NodeId, SessionId,
};

/// The run configuration every property test uses: proptest's own default
/// (256 cases, or `PROPTEST_CASES` when set: proptest 1.12 reads it in
/// `Config::default()`), with no regression files. A failure prints its
/// minimal counterexample; a property that fails is kept `#[ignore]`d with
/// that counterexample in its reason, not persisted under `src/` (which
/// ships in the crate tarball). The suite runs in every CI row, so search
/// harder locally with `PROPTEST_CASES=10000`.
pub fn config() -> Config {
    Config {
        failure_persistence: None,
        ..Config::default()
    }
}

/// The session every builder here writes to.
pub fn sid() -> SessionId {
    SessionId::from("prop-session")
}

/// A fixed instant, so nothing depends on the wall clock.
pub fn t0() -> DateTime<Utc> {
    Utc.timestamp_opt(1_752_000_000, 0).unwrap()
}

/// Concept ids: `Uuid` pair `(2, n)`, so id order is `n` order.
pub fn cid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(2, n))
}

/// The one interaction every concept here derives from: pair `(1, 1)`,
/// disjoint from every concept id.
pub fn origin() -> NodeId {
    NodeId(Uuid::from_u64_pair(1, 1))
}

/// A concept with the given content, key and status, derived from
/// [`origin`].
pub fn concept(n: u64, content: &str, key: &str, status: CanonizationStatus) -> Concept {
    Concept {
        id: cid(n),
        session_id: sid(),
        content: content.into(),
        canonical_key: key.into(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin(),
        origin_agent: AgentId::from("prop-agent"),
        created_at: t0(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: status,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    }
}

/// A graph holding [`origin`] and the given concepts.
pub fn graph_of(concepts: impl IntoIterator<Item = Concept>) -> Graph {
    let mut g = Graph::new(sid());
    g.insert_interaction(Interaction {
        id: origin(),
        session_id: sid(),
        agent_id: AgentId::from("prop-agent"),
        prompt_text: Some("origin".into()),
        previous_id: None,
        created_at: t0(),
        event_time: None,
    })
    .expect("origin interaction");
    for c in concepts {
        g.insert_concept(c, origin()).expect("concept inserts");
    }
    g
}

/// A graph holding [`origin`] and concepts `0..n`, each keyed `c<n>` and
/// `None` status.
pub fn plain_graph(n: u64) -> Graph {
    graph_of((0..n).map(|i| {
        let text = format!("c{i}");
        concept(i, &text, &text, CanonizationStatus::None)
    }))
}

/// A canonical key as the ranking comparators see one: none, a plain key, a
/// scoped (`RepoResource:v1:`) key, or a plain key spelled like a scoped
/// key's tie form, so `tie_form` collisions are drawn too. Short alphabets
/// make ties likely.
pub fn key() -> impl Strategy<Value = Option<String>> {
    let text = "[aAbZ:]{0,3}";
    prop_oneof![
        Just(None),
        text.prop_map(Some),
        text.prop_map(|t| Some(format!("{RESOURCE_KEY_PREFIX}{t}"))),
        text.prop_map(|t| Some(format!("reporesource:v1:{t}"))),
    ]
}

/// The tie form, spelled out from its definition (#129) rather than taken
/// from `resource::tie_form`, so the property tests have an oracle of their
/// own: a key under the scoped prefix `RepoResource:v1:` compares with that
/// ASCII prefix lowercased; every other key is itself. Lean leaves the key
/// order abstract (`keyCmp` in `formal/LamboProofs/Order.lean`); this is the
/// instance Lambo uses.
pub fn spec_tie_form(key: &str) -> String {
    match key.strip_prefix("RepoResource:v1:") {
        Some(rest) => format!("reporesource:v1:{rest}"),
        None => key.to_owned(),
    }
}

/// [`spec_tie_key`]'s value.
pub type SpecTieKey = (bool, Option<String>, Uuid);

/// Lean `Order.tieBreak` (`lex (byKey key (optCmp keyCmp)) idCmp`,
/// `formal/LamboProofs/Order.lean`) as a derived `Ord` key: keyed members
/// first (`optCmp`: `some _ < none`, so `is_none()` ascending), then the key
/// in [`spec_tie_form`] ascending, then the id ascending.
pub fn spec_tie_key(key: Option<&str>, id: NodeId) -> SpecTieKey {
    (key.is_none(), key.map(spec_tie_form), id.0)
}
