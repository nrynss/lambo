//! What `GET /admin/sessions` reads of the registry (#32 PR 7, design
//! §6.3): one row per session this serve hosts or holds a slot for, with
//! its state and, for an attached session, its size. The admin route
//! filters the rows by the caller's scope; nothing here knows about
//! credentials.

use serde::Serialize;

use super::{SessionRegistry, Slot};

/// One session's row in `/admin/sessions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(in crate::mcp::serve) struct SlotView {
    /// The session id.
    pub(in crate::mcp::serve) session: String,
    /// `attaching` (on demand, #32 PR 6), `live`, `detaching`,
    /// `held_elsewhere`, `failed`, `erasing`, `erased`, or `unattached`
    /// (pinned, in no slot: between states).
    pub(in crate::mcp::serve) state: &'static str,
    /// Whether the serve pins it (attached at startup, never evicted).
    pub(in crate::mcp::serve) pinned: bool,
    /// Whether `/mcp` serves it.
    pub(in crate::mcp::serve) default: bool,
    /// Attached sessions only: their size in this process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::mcp::serve) attached: Option<AttachedView>,
}

/// An attached session's size (design §3.6: reported, not enforced).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(in crate::mcp::serve) struct AttachedView {
    pub(in crate::mcp::serve) nodes: usize,
    pub(in crate::mcp::serve) edges: usize,
    pub(in crate::mcp::serve) concepts: usize,
    pub(in crate::mcp::serve) embedded_concepts: usize,
    /// The bytes of the f32 vectors the graph holds in RAM (#8):
    /// `embedded_concepts × dim × 4`. The estimate the design names; graph
    /// structure and indexes are not counted.
    pub(in crate::mcp::serve) estimated_vector_bytes: u64,
    /// The write-behind log not yet flushed (mutations).
    pub(in crate::mcp::serve) log_depth: usize,
}

impl SessionRegistry {
    /// A row for every hosted session, in pinned order, then for every
    /// other id that holds a slot, sorted. Reads RAM only: no store call.
    pub(in crate::mcp::serve) fn slot_views(&self) -> Vec<SlotView> {
        // The states and the live handles under the slots lock; the sizes
        // after it, so no graph lock is ever taken inside it.
        let rows: Vec<(String, &'static str, Option<std::sync::Arc<_>>)> = {
            let slots = self.slots.lock();
            let mut others: Vec<&String> =
                slots.keys().filter(|id| !self.order.contains(id)).collect();
            others.sort();
            self.order
                .iter()
                .chain(others)
                .map(|id| match slots.get(id) {
                    Some(Slot::Live(session)) => {
                        (id.clone(), "live", Some(std::sync::Arc::clone(session)))
                    }
                    Some(Slot::Attaching { .. }) => (id.clone(), "attaching", None),
                    Some(Slot::Detaching) => (id.clone(), "detaching", None),
                    Some(Slot::HeldElsewhere { .. }) => (id.clone(), "held_elsewhere", None),
                    Some(Slot::Failed) => (id.clone(), "failed", None),
                    Some(Slot::Erasing) => (id.clone(), "erasing", None),
                    Some(Slot::Erased) => (id.clone(), "erased", None),
                    None => (id.clone(), "unattached", None),
                })
                .collect()
        };
        rows.into_iter()
            .map(|(id, state, live)| {
                let attached = live.map(|session: std::sync::Arc<super::AttachedSession>| {
                    let stats = session.mem.stats();
                    let dim = session.mem.embedding_contract().dim as u64;
                    AttachedView {
                        nodes: stats.node_count,
                        edges: stats.edge_count,
                        concepts: stats.concept_count,
                        embedded_concepts: stats.embedded_concepts,
                        estimated_vector_bytes: stats.embedded_concepts as u64 * dim * 4,
                        log_depth: stats.log_depth,
                    }
                });
                SlotView {
                    pinned: self.order.contains(&id),
                    default: self.default.as_deref() == Some(id.as_str()),
                    session: id,
                    state,
                    attached,
                }
            })
            .collect()
    }
}
