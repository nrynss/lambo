//! Unit tests for the SQLite adapter, grouped by subject.

use super::*;
use crate::graph::demote::demote;
use crate::graph::derive::{derive, ParentOf};
use crate::graph::reserve::reserve;
use crate::store::load::{load_session, load_session_async};
#[cfg(feature = "store-memory")]
use crate::store::memory::MemoryStore;
use crate::types::{AgentId, CanonizationStatus, ConceptType, EdgeType, Node as NodeKind};
use chrono::TimeZone;

mod access;
mod connection;
mod erase;
mod leases;
mod persistence;
mod schema;
mod structural;
mod vectors;
// #8: the holder's graph-backed vector source against the SQLite scan.
mod vector_graph_parity;

fn test_store() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:").unwrap()
}

fn plant_concept(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    concept_type: ConceptType,
    ts: DateTime<Utc>,
) -> Mutation {
    Mutation::UpsertNode {
        node: NodeKind::Concept(Concept {
            id,
            session_id: sid.clone(),
            content: content.into(),
            canonical_key: content.to_lowercase(),
            concept_type,
            origin_interaction: origin,
            origin_agent: AgentId::from("a"),
            created_at: ts,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        }),
    }
}

fn plant_interaction(
    sid: &SessionId,
    id: NodeId,
    prev: Option<NodeId>,
    ts: DateTime<Utc>,
) -> Mutation {
    Mutation::UpsertNode {
        node: NodeKind::Interaction(Interaction {
            event_time: None,
            id,
            session_id: sid.clone(),
            agent_id: AgentId::from("a"),
            prompt_text: Some("prompt".into()),
            previous_id: prev,
            created_at: ts,
        }),
    }
}

// -- F1/F2 vector search (issue #5) -------------------------------------

/// The 8-d contract every vector test below writes and queries under.
fn vec_contract(dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: Some("test-model".into()),
        dim,
    }
}

/// A store whose reported width matches `dim`, so a small test vector is a
/// legal probe (the default width is the `[embedder] dim` default, not 8).
fn vec_test_store(dim: usize) -> SqliteStore {
    SqliteStore::connect("sqlite::memory:")
        .unwrap()
        .with_vector_dim(dim)
        .unwrap()
}

/// Snapshot -> mutation batch (nodes then edges; §2.4 order). The
/// fixtures carry no canonization events; synonyms and reservations are
/// RAM-local (S5) and never part of the structural queries.
#[cfg(feature = "fixtures")]
fn snapshot_to_batch(snap: &GraphSnapshot) -> MutationBatch {
    let mut batch = MutationBatch::new();
    for i in &snap.interactions {
        batch.push(Mutation::UpsertNode {
            node: NodeKind::Interaction(i.clone()),
        });
    }
    for c in &snap.concepts {
        batch.push(Mutation::UpsertNode {
            node: NodeKind::Concept(c.clone()),
        });
    }
    for e in &snap.edges {
        batch.push(Mutation::UpsertEdge { edge: e.clone() });
    }
    batch
}

/// A deterministic **unit-norm** vector for fixture concept `i`.
///
/// Unit norm is the `Embedder` output contract (see `embed::Embedder::embed`), and
/// the property the SQLite/Cockroach score identity rests on, so synthetic vectors
/// that stand in for embedder output must honour it too. The spread is deliberately
/// uneven — not one-hot — so distinct concepts produce distinct, non-orthogonal
/// scores and a ranking has something to get wrong.
#[cfg(feature = "fixtures")]
fn synthetic_unit_vector(i: usize, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim)
        .map(|j| {
            // Cheap deterministic spread; no rand dependency, stable across runs
            // and platforms (integer math, then one divide).
            let k = ((i + 1) * 37 + (j + 1) * 11) % 97;
            (k as f32) / 97.0 - 0.5
        })
        .collect();
    // Guard the degenerate case rather than dividing by ~0.
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(norm > 1e-6, "degenerate synthetic vector for i={i}");
    for x in &mut v {
        *x /= norm;
    }
    v
}

// -----------------------------------------------------------------------
// H1 — cross-store recall parity harness
// (dev-diary/lambo-for-mooshik/H-cross-store-parity.md)
// -----------------------------------------------------------------------
//
// The matrix above (F's Done-when box 5, cluster-free half) proves SQLite
// agrees with a hand-rolled, non-`GraphStore` cosine oracle. H1 asks a
// narrower, harder question: do two independent *stores* — real
// `GraphStore` adapters, driven only through the public
// `vector_candidates_checked` surface — agree with EACH OTHER on the same
// seeded graph? That is "cross-store" rather than "adapter-vs-formula".
//
// H2 (live Cockroach, needs `LAMBO_COCKROACH_DSN`) stays a twin in
// cockroach.rs. H3 (pgvector) slots in here by appending to
// `build_adapters` when a DSN is offered: the pairwise loop, the
// measures, and the report schema need no second harness. See the
// doc comment on `ParityReport` for why.
#[cfg(feature = "fixtures")]
mod h1_cross_store_parity;

/// A scratch sqlite file path this test owns; the returned guard removes the
/// dir on drop, so the caller only has to keep it alive.
fn scratch_db() -> (crate::test_util::ScratchDir, String) {
    let dir = crate::test_util::ScratchDir::new("lambo-lease");
    let path = dir.join("lease.sqlite");
    let s = path.to_str().unwrap().to_string();
    (dir, s)
}

// -----------------------------------------------------------------------
// F1/F2 end to end: derive -> flush -> vector recall, on SQLite
// -----------------------------------------------------------------------

/// Everything the end-to-end test needs beyond `store-sqlite`: a deterministic
/// embedder with a documented near/far pair.
#[cfg(feature = "embed-fixture")]
mod vector_e2e;
