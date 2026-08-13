//! The seven MCP tools of `lambo serve` (spec §6.2).
//!
//! One process owns the session (spec §2.2); every tool call is a task inside
//! it. [`LamboServer`] is a cheap handle — cloning it clones an `Arc<Memory>`,
//! never a second [`Memory`] (a second one would spawn a rival task trio
//! against a divergent RAM copy of the same session).
//!
//! # F18 — timestamps are server-side, always
//!
//! **No tool in this module accepts a timestamp, and none may ever be added.**
//! `derive` / `record_action` / `demote` take their logical timestamp from the
//! interaction node, which `Memory::begin_interaction` stamps with `Utc::now()`
//! on the server. A client-supplied timestamp would propagate to every concept
//! and edge below that interaction, and backdating by 61s would turn the whole
//! `canonization_edge_min_age` inflation guard into a no-op (P6 review F18).
//!
//! # Error convention
//!
//! Per rmcp's own guidance: `Err(ErrorData)` is for requests the server cannot
//! route (the client renders those opaquely, so the message never reaches the
//! user); `Ok(CallToolResult::error(..))` is for "the tool ran and it did not
//! work", whose content the caller actually sees. Memory-level failures —
//! conflicts, unknown nodes, a closed session — are the latter.

use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::json;

use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::memory::Memory;
use crate::recall::format;
use crate::types::{ConceptType, EdgeType, LamboError, NodeId, RecallQuery};

/// Upper bound on `top_k` a client may ask for. Recall assembles and renders
/// every hit, so an unbounded `top_k` from one client is a cheap way to stall
/// the single process every other client shares.
const MAX_TOP_K: usize = 100;
/// Upper bound on `traversal_depth` (spec §8 phase 2 is a BFS — depth is an
/// exponent, not a linear cost).
const MAX_TRAVERSAL_DEPTH: usize = 5;
/// Upper bound on `max_tokens` for one context block.
const MAX_MAX_TOKENS: usize = 100_000;
/// Upper bound on concepts in a single `lambo_derive` call.
const MAX_CONCEPTS_PER_DERIVE: usize = 64;
/// Upper bound on `lambo_reserve` TTL — a soft lock (spec §11), not a lease.
const MAX_RESERVE_TTL_SECS: u64 = 3600;
/// Upper bound on `lambo_inspect` depth.
const MAX_INSPECT_DEPTH: usize = 5;
/// Cap on neighbours rendered per `lambo_inspect` frontier level.
const MAX_INSPECT_NODES: usize = 200;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// Concept type as it crosses the wire. Mirrors [`ConceptType`] rather than
/// deriving `JsonSchema` on the core type, so the MCP schema is owned here and
/// a core rename cannot silently change a published tool schema.
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WireConceptType {
    Entity,
    Logic,
    Constraint,
    Resource,
    Observation,
}

impl From<WireConceptType> for ConceptType {
    fn from(w: WireConceptType) -> Self {
        match w {
            WireConceptType::Entity => ConceptType::Entity,
            WireConceptType::Logic => ConceptType::Logic,
            WireConceptType::Constraint => ConceptType::Constraint,
            WireConceptType::Resource => ConceptType::Resource,
            WireConceptType::Observation => ConceptType::Observation,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallParams {
    /// Calling agent (spec §2.2 — see the attribution note in the tool docs).
    pub agent_id: String,
    /// Natural-language query.
    pub query: String,
    /// Hits to return. Defaults to the session config's `default_top_k`.
    pub top_k: Option<usize>,
    /// Token budget for the rendered context block.
    pub max_tokens: Option<usize>,
    /// Graph traversal depth for phase 2 expansion.
    pub traversal_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WireConcept {
    /// The concept text.
    pub content: String,
    /// One of `entity`, `logic`, `constraint`, `resource`, `observation`.
    pub concept_type: WireConceptType,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WireParentOf {
    pub parent: String,
    pub child: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeriveParams {
    pub agent_id: String,
    /// Concepts to derive from this interaction.
    pub concepts: Vec<WireConcept>,
    /// Optional `(parent, child)` hierarchy pairs. Both ends resolve (and may
    /// be created) as concepts.
    pub parent_of: Option<Vec<WireParentOf>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordActionParams {
    pub agent_id: String,
    /// The action taken — becomes a `Resource` concept.
    pub action: String,
    /// Resources this action creates (`Causal` edges).
    pub produces: Option<Vec<String>>,
    /// Resources this action mutates (`Causal` edges).
    pub modifies: Option<Vec<String>>,
    /// Things this action depends on (`Dependency` edges).
    pub depends_on: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReserveParams {
    pub agent_id: String,
    /// Node to reserve, as a UUID string (from `lambo_recall` or
    /// `lambo_inspect`).
    pub node_id: String,
    /// Soft-lock lifetime in seconds (default 30, max 3600).
    pub ttl_seconds: Option<u64>,
    /// Release this agent's existing soft lock instead of taking one.
    pub release: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    pub agent_id: String,
    /// Concept content (or a node UUID) to centre the neighbourhood on.
    pub focus: String,
    /// Hops out from the focus (default 2, max 5).
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SaintsParams {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatsParams {
    pub agent_id: String,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// MCP surface over one [`Memory`].
#[derive(Clone)]
pub struct LamboServer {
    mem: Arc<Memory>,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for LamboServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LamboServer")
            .field("session", self.mem.session())
            .field("agent", self.mem.agent())
            .finish_non_exhaustive()
    }
}

/// Render a `Memory` failure as a caller-visible tool error.
fn tool_err(what: &str, err: LamboError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{what}: {err}"))])
}

/// Reject a parameter the server will not act on.
///
/// A bad parameter is the *client's* problem and it is worth surfacing where
/// the model can read and correct it, so this is a tool-level error rather than
/// a `-32602` the client renders opaquely.
fn bad_param(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

impl LamboServer {
    /// Wrap a live [`Memory`]. The `Arc` is the point: every clone of this
    /// server — one per HTTP request, in the streamable-http transport — shares
    /// the single session owner.
    pub fn new(mem: Arc<Memory>) -> Self {
        Self {
            mem,
            tool_router: Self::tool_router(),
        }
    }

    /// The session this process owns.
    pub fn memory(&self) -> &Arc<Memory> {
        &self.mem
    }

    /// Validate `agent_id` and report the attribution gap honestly.
    ///
    /// Every tool carries `agent_id` because spec §6.2/§2.2 says calls from
    /// several MCP clients are tasks in one process, each identifying itself.
    /// **`Memory` binds a single `AgentId` at `build()` and exposes no
    /// per-call override**, so graph-level attribution (the `agent_id` written
    /// onto interactions, and the identity `reserve` contends on) is the
    /// process agent, not this one. Rather than silently discard the caller's
    /// identity, a mismatch is returned as a warning on the result. See the
    /// T8.2 Handoff Log entry — closing this needs a `Memory` change, which is
    /// not T8.2's to make.
    fn attribution(&self, agent_id: &str) -> Result<Vec<String>, CallToolResult> {
        if agent_id.trim().is_empty() {
            return Err(bad_param("agent_id must be a non-empty string"));
        }
        let owner = self.mem.agent().0.as_str();
        if agent_id == owner {
            Ok(Vec::new())
        } else {
            Ok(vec![format!(
                "attribution: this process owns the session as agent '{owner}'; \
                 the call from '{agent_id}' is recorded in the graph as '{owner}'. \
                 Per-call agent attribution needs a Memory-level agent override \
                 (see T8.2 Handoff Log)."
            )])
        }
    }
}

#[tool_router]
impl LamboServer {
    /// Three-phase recall (spec §8) rendered as the T5.3 context block.
    ///
    /// The block is returned verbatim as text content — it is the artifact the
    /// calling agent is meant to read — with the hits and warnings alongside as
    /// structured content.
    #[tool(
        name = "lambo_recall",
        description = "Recall relevant memory for a query and return the Lambo context block \
                       (canonical markers, blast-radius warnings, conflict lines)."
    )]
    async fn lambo_recall(&self, Parameters(p): Parameters<RecallParams>) -> CallToolResult {
        let mut warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.query.trim().is_empty() {
            return bad_param("query must be a non-empty string");
        }
        let cfg = self.mem.config();
        let top_k = p.top_k.unwrap_or(cfg.default_top_k);
        let max_tokens = p.max_tokens.unwrap_or(cfg.default_max_tokens);
        let traversal_depth = p.traversal_depth.unwrap_or(cfg.default_traversal_depth);
        if top_k == 0 || top_k > MAX_TOP_K {
            return bad_param(format!("top_k must be in 1..={MAX_TOP_K}"));
        }
        if traversal_depth > MAX_TRAVERSAL_DEPTH {
            return bad_param(format!(
                "traversal_depth must be in 0..={MAX_TRAVERSAL_DEPTH}"
            ));
        }
        if max_tokens == 0 || max_tokens > MAX_MAX_TOKENS {
            return bad_param(format!("max_tokens must be in 1..={MAX_MAX_TOKENS}"));
        }

        let query = RecallQuery {
            query: p.query,
            top_k,
            max_tokens,
            traversal_depth,
        };
        let result = match self.mem.recall(query).await {
            Ok(r) => r,
            Err(e) => return tool_err("lambo_recall", e),
        };
        warnings.extend(result.warnings.iter().cloned());

        let hits: Vec<_> = result
            .hits
            .iter()
            .map(|h| {
                json!({
                    "node_id": h.node_id.0.to_string(),
                    "content": h.content,
                    "concept_type": h.concept_type,
                    "score": h.score,
                    "is_canonical": h.is_canonical,
                    "blast_radius": h.blast_radius,
                })
            })
            .collect();

        let mut out = CallToolResult::success(vec![ContentBlock::text(result.context.clone())]);
        out.structured_content = Some(json!({
            "context": result.context,
            "hits": hits,
            "warnings": warnings,
        }));
        out
    }

    /// Derive concepts from a fresh interaction (spec §7).
    ///
    /// The interaction's `created_at` is stamped server-side (F18) — this tool
    /// takes no timestamp.
    #[tool(
        name = "lambo_derive",
        description = "Derive concepts from the current interaction into session memory. \
                       Timestamps are stamped server-side; do not send one."
    )]
    async fn lambo_derive(&self, Parameters(p): Parameters<DeriveParams>) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.concepts.is_empty() {
            return bad_param("concepts must contain at least one entry");
        }
        if p.concepts.len() > MAX_CONCEPTS_PER_DERIVE {
            return bad_param(format!(
                "concepts must contain at most {MAX_CONCEPTS_PER_DERIVE} entries"
            ));
        }
        if let Some(bad) = p.concepts.iter().find(|c| c.content.trim().is_empty()) {
            let _ = bad;
            return bad_param("every concept.content must be a non-empty string");
        }

        // `derive` borrows `&[(&str, ConceptType)]` and `ParentOf<'_>` borrows
        // `&[(&str, &str)]`; both owners must outlive the await.
        let concepts: Vec<(&str, ConceptType)> = p
            .concepts
            .iter()
            .map(|c| (c.content.as_str(), ConceptType::from(c.concept_type)))
            .collect();
        let pairs: Vec<(&str, &str)> = p
            .parent_of
            .iter()
            .flatten()
            .map(|r| (r.parent.as_str(), r.child.as_str()))
            .collect();
        if pairs
            .iter()
            .any(|(a, b)| a.trim().is_empty() || b.trim().is_empty())
        {
            return bad_param("parent_of entries must have non-empty parent and child");
        }
        let parent_of = if pairs.is_empty() {
            ParentOf::none()
        } else {
            ParentOf::from_pairs(&pairs)
        };

        let outcome = match self.mem.derive(&concepts, &parent_of).await {
            Ok(o) => o,
            Err(e) => return tool_err("lambo_derive", e),
        };

        let created: Vec<String> = outcome.created.iter().map(|n| n.0.to_string()).collect();
        let matched: Vec<String> = outcome.matched.iter().map(|n| n.0.to_string()).collect();
        let summary = format!(
            "derived {} concept(s): {} created, {} matched existing",
            concepts.len(),
            created.len(),
            matched.len()
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        out.structured_content = Some(json!({
            "summary": summary,
            "created": created,
            "matched": matched,
            "warnings": warnings,
        }));
        out
    }

    /// Record an agent action (spec §7) — a `Resource` concept plus `Causal` /
    /// `Dependency` edges, on a fresh server-stamped interaction (F18).
    #[tool(
        name = "lambo_record_action",
        description = "Record an action the agent took, with what it produces, modifies and \
                       depends on. Timestamps are stamped server-side; do not send one."
    )]
    async fn lambo_record_action(
        &self,
        Parameters(p): Parameters<RecordActionParams>,
    ) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.action.trim().is_empty() {
            return bad_param("action must be a non-empty string");
        }
        let produces: Vec<&str> = p.produces.iter().flatten().map(String::as_str).collect();
        let modifies: Vec<&str> = p.modifies.iter().flatten().map(String::as_str).collect();
        let depends_on: Vec<&str> = p.depends_on.iter().flatten().map(String::as_str).collect();
        if produces
            .iter()
            .chain(&modifies)
            .chain(&depends_on)
            .any(|s| s.trim().is_empty())
        {
            return bad_param("produces / modifies / depends_on entries must be non-empty");
        }

        let action = Action {
            action: p.action.as_str(),
            produces: &produces,
            modifies: &modifies,
            depends_on: &depends_on,
        };
        let outcome = match self.mem.record_action(&action) {
            Ok(o) => o,
            Err(e) => return tool_err("lambo_record_action", e),
        };

        let created: Vec<String> = outcome.created.iter().map(|n| n.0.to_string()).collect();
        let summary = format!(
            "recorded action '{}': {} concept(s) created, {} edge(s) added",
            p.action,
            created.len(),
            outcome.edges
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        out.structured_content = Some(json!({
            "summary": summary,
            "action_node": outcome.action_node.0.to_string(),
            "created": created,
            "edges": outcome.edges,
            "warnings": warnings,
        }));
        out
    }

    /// Take (or release) a soft lock on a node — spec §11.
    ///
    /// Not durable: reservations are RAM-local to this process (pinned contract
    /// S5). "No reservation" after a restart does **not** mean nobody else is
    /// working on the node.
    #[tool(
        name = "lambo_reserve",
        description = "Take a soft lock on a memory node before editing it (or release one). \
                       Reservations are advisory and do not survive a server restart."
    )]
    async fn lambo_reserve(&self, Parameters(p): Parameters<ReserveParams>) -> CallToolResult {
        let mut warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        let node_id = match uuid::Uuid::parse_str(p.node_id.trim()) {
            Ok(u) => NodeId(u),
            Err(e) => return bad_param(format!("node_id must be a UUID: {e}")),
        };

        if p.release.unwrap_or(false) {
            return match self.mem.release(node_id) {
                Ok(()) => {
                    let msg = format!("released {}", node_id.0);
                    let mut out = CallToolResult::success(vec![ContentBlock::text(msg.clone())]);
                    out.structured_content =
                        Some(json!({ "released": true, "node_id": node_id.0.to_string(),
                                     "summary": msg, "warnings": warnings }));
                    out
                }
                Err(e) => tool_err("lambo_reserve (release)", e),
            };
        }

        let ttl_secs = p.ttl_seconds.unwrap_or(30);
        if ttl_secs == 0 || ttl_secs > MAX_RESERVE_TTL_SECS {
            return bad_param(format!("ttl_seconds must be in 1..={MAX_RESERVE_TTL_SECS}"));
        }
        let reservation = match self.mem.reserve(node_id, Duration::from_secs(ttl_secs)) {
            Ok(r) => r,
            Err(e) => return tool_err("lambo_reserve", e),
        };
        warnings.push(
            "reservations are advisory and RAM-local: they are lost on server restart".into(),
        );
        let summary = format!(
            "reserved {} until {} for agent '{}'",
            node_id.0,
            reservation.expires_at.to_rfc3339(),
            reservation.agent_id.0
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        out.structured_content = Some(json!({
            "summary": summary,
            "node_id": node_id.0.to_string(),
            "agent_id": reservation.agent_id.0,
            "expires_at": reservation.expires_at.to_rfc3339(),
            "warnings": warnings,
        }));
        out
    }

    /// Neighbourhood around a focus concept — the read-only graph view.
    #[tool(
        name = "lambo_inspect",
        description = "Inspect the neighbourhood around a concept: its type, canonization \
                       status, blast radius and typed edges out to a depth."
    )]
    async fn lambo_inspect(&self, Parameters(p): Parameters<InspectParams>) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.focus.trim().is_empty() {
            return bad_param("focus must be a non-empty string");
        }
        let depth = p.depth.unwrap_or(2);
        if depth > MAX_INSPECT_DEPTH {
            return bad_param(format!("depth must be in 0..={MAX_INSPECT_DEPTH}"));
        }

        // One short read section, no `.await` inside (spec §6.4).
        let rendered = {
            let g = self.mem.graph().read();
            let focus = p.focus.trim();
            // A UUID that names a live node wins; otherwise exact content
            // (case-insensitive), then a substring match.
            let target = uuid::Uuid::parse_str(focus)
                .ok()
                .map(NodeId)
                .filter(|id| g.node(*id).is_some())
                .or_else(|| {
                    let needle = focus.to_lowercase();
                    g.concepts()
                        .find(|c| c.content.eq_ignore_ascii_case(focus))
                        .or_else(|| {
                            g.concepts()
                                .find(|c| c.content.to_lowercase().contains(&needle))
                        })
                        .map(|c| c.id)
                });
            target.map(|t| render_neighbourhood(&g, t, depth))
        };

        let Some((text, structured)) = rendered else {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "lambo_inspect: no concept matching '{}' in session '{}'",
                p.focus,
                self.mem.session().0
            ))]);
        };

        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        out.structured_content = Some(json!({
            "view": text,
            "focus": structured,
            "warnings": warnings,
        }));
        out
    }

    /// The canonical ("saints") memories — spec §10.
    #[tool(
        name = "lambo_saints",
        description = "List the session's canonical memories — concepts that earned Canonical \
                       status through the audited transition path."
    )]
    async fn lambo_saints(&self, Parameters(p): Parameters<SaintsParams>) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        let saints = self.mem.canonical_memories();
        let mut text = format!(
            "{} canonical memor{} in session '{}'\n",
            saints.len(),
            if saints.len() == 1 { "y" } else { "ies" },
            self.mem.session().0
        );
        for s in &saints {
            text.push_str(&format!(
                "  {} [{:?}, canonical]  blast_radius={}  accesses={}  since {}\n",
                s.content,
                s.concept_type,
                s.blast_radius,
                s.access_count,
                s.created_at.to_rfc3339()
            ));
        }
        let rows: Vec<_> = saints
            .iter()
            .map(|s| {
                json!({
                    "node_id": s.node_id.0.to_string(),
                    "content": s.content,
                    "concept_type": s.concept_type,
                    "blast_radius": s.blast_radius,
                    "access_count": s.access_count,
                    "created_at": s.created_at.to_rfc3339(),
                })
            })
            .collect();
        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        out.structured_content = Some(json!({
            "summary": text,
            "saints": rows,
            "warnings": warnings,
        }));
        out
    }

    /// Session health — the spec §2.4 observable durability bound.
    #[tool(
        name = "lambo_stats",
        description = "Session health: flush lag, write-behind log depth, node/edge/concept \
                       counts, canonization progress and degraded state."
    )]
    async fn lambo_stats(&self, Parameters(p): Parameters<StatsParams>) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        let s = self.mem.stats();
        let text = format!(
            "session '{}' (owner agent '{}')\n\
             nodes={} edges={} concepts={} canonical={}\n\
             flush_lag={:?} log_depth={} flush_depth={} dead_lettered={} degraded={}\n\
             epoch={} daemon_cycles={} canonization_cycles={} canonization_failures={}",
            s.session.0,
            s.agent.0,
            s.node_count,
            s.edge_count,
            s.concept_count,
            s.canonical_count,
            s.flush_lag,
            s.log_depth,
            s.flush_depth,
            s.dead_lettered,
            s.degraded,
            s.epoch,
            s.daemon_cycles,
            s.canonization_cycles,
            s.canonization_failures,
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        out.structured_content = Some(json!({
            "summary": text,
            "session": s.session.0,
            "agent": s.agent.0,
            "flush_lag_ms": s.flush_lag.as_millis() as u64,
            "log_depth": s.log_depth,
            "flush_depth": s.flush_depth,
            "dead_lettered": s.dead_lettered,
            "degraded": s.degraded,
            "node_count": s.node_count,
            "edge_count": s.edge_count,
            "concept_count": s.concept_count,
            "canonical_count": s.canonical_count,
            "epoch": s.epoch,
            "daemon_cycles": s.daemon_cycles,
            "canonization_cycles": s.canonization_cycles,
            "canonization_failures": s.canonization_failures,
            "warnings": warnings,
        }));
        out
    }
}

/// Render a BFS neighbourhood around `target`. Caller holds the graph read
/// lock; this function never awaits (spec §6.4).
fn render_neighbourhood(
    g: &crate::graph::Graph,
    target: NodeId,
    depth: usize,
) -> (String, serde_json::Value) {
    use std::collections::{HashMap, HashSet};

    let radii = format::blast_radii(g);
    let label = |id: NodeId| -> String {
        match g.node(id) {
            Some(crate::types::Node::Concept(c)) => {
                let canon = match c.canonization_status {
                    crate::types::CanonizationStatus::Canonical => ", canonical",
                    crate::types::CanonizationStatus::Venerable => ", venerable",
                    crate::types::CanonizationStatus::Candidate => ", candidate",
                    crate::types::CanonizationStatus::None => "",
                };
                format!("{} [{:?}{}]", c.content, c.concept_type, canon)
            }
            Some(crate::types::Node::Interaction(i)) => {
                format!("<interaction {}>", i.id.0)
            }
            None => format!("<missing {}>", id.0),
        }
    };

    let mut text = String::new();
    text.push_str(&format!("focus: {}\n", label(target)));
    if let Some(r) = radii.get(&target) {
        text.push_str(&format!("blast radius: {r}\n"));
        if *r > 0 {
            text.push_str(&format!("{}\n", format::blast_radius_warning(*r)));
        }
    }
    if let Some(res) = g.reservation(target) {
        text.push_str(&format!("{}\n", format::reservation_warning(res)));
    }

    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(target);
    let mut frontier = vec![target];
    let mut levels: Vec<serde_json::Value> = Vec::new();
    let mut budget = MAX_INSPECT_NODES;

    for hop in 1..=depth {
        let mut next = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut by_type: HashMap<EdgeType, Vec<String>> = HashMap::new();
        for &node in &frontier {
            for edge in g.incident_edges(node) {
                let other = if edge.source == node {
                    edge.target
                } else {
                    edge.source
                };
                if !seen.insert(other) {
                    continue;
                }
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let dir = if edge.source == node { "->" } else { "<-" };
                by_type
                    .entry(edge.edge_type)
                    .or_default()
                    .push(format!("{dir} {}", label(other)));
                rows.push(json!({
                    "node_id": other.0.to_string(),
                    "label": label(other),
                    "edge_type": format!("{:?}", edge.edge_type),
                    "direction": dir,
                    "weight": edge.weight,
                }));
                next.push(other);
            }
        }
        if rows.is_empty() {
            break;
        }
        text.push_str(&format!("\nhop {hop}:\n"));
        let mut kinds: Vec<_> = by_type.into_iter().collect();
        kinds.sort_by_key(|(k, _)| format!("{k:?}"));
        for (kind, mut entries) in kinds {
            entries.sort();
            text.push_str(&format!("  {kind:?}\n"));
            for e in entries {
                text.push_str(&format!("    {e}\n"));
            }
        }
        levels.push(json!({ "hop": hop, "neighbours": rows }));
        frontier = next;
        if budget == 0 {
            text.push_str(&format!(
                "\n(truncated at {MAX_INSPECT_NODES} neighbours)\n"
            ));
            break;
        }
    }

    let structured = json!({
        "node_id": target.0.to_string(),
        "label": label(target),
        "blast_radius": radii.get(&target).copied().unwrap_or(0),
        "levels": levels,
    });
    (text, structured)
}

// `router = self.tool_router` on purpose: the macro's default is
// `Self::tool_router()`, which **rebuilds the whole router — every tool's JSON
// schema included — on every `tools/list` and every `tools/call`**. Pointing it
// at the field built once in `new()` keeps per-call work to a map lookup.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for LamboServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` / `Implementation` are `#[non_exhaustive]`: start from
        // the SDK's default (which carries the negotiated protocol version) and
        // set only what is ours.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("lambo", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(format!(
            "Lambo agentic graph memory for session '{}'. Call lambo_recall before \
                 acting on a task to load relevant prior memory, lambo_derive and \
                 lambo_record_action to write what you learned and did, lambo_reserve \
                 before editing a shared concept, and lambo_inspect / lambo_saints / \
                 lambo_stats to look around. Every tool takes your agent_id. Never send \
                 a timestamp: the server stamps them.",
            self.mem.session().0
        ));
        info
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::store::{GraphStore, MemoryStore};
    use crate::types::EmbeddingContract;

    async fn server(session: &str) -> LamboServer {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Memory::builder()
            .session(session)
            .agent("agent-a")
            // Keep the background flush loop out of the assertions.
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .build()
            .await
            .expect("build");
        LamboServer::new(Arc::new(mem))
    }

    fn tools(s: &LamboServer) -> Vec<rmcp::model::Tool> {
        s.tool_router.list_all()
    }

    #[tokio::test]
    async fn the_router_publishes_exactly_the_seven_spec_tools() {
        let s = server("mcp-seven").await;
        let mut names: Vec<String> = tools(&s).iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "lambo_derive",
                "lambo_inspect",
                "lambo_recall",
                "lambo_record_action",
                "lambo_reserve",
                "lambo_saints",
                "lambo_stats",
            ],
            "spec §6.2 names the seven tools exactly; adding or renaming one is a spec change"
        );
        s.mem.close().await.expect("close");
    }

    /// Every tool must advertise a usable object schema with `agent_id`, or a
    /// client cannot call it correctly (spec §2.2 — calls carry `agent_id`).
    #[tokio::test]
    async fn every_tool_schema_is_an_object_requiring_agent_id() {
        let s = server("mcp-schemas").await;
        for t in tools(&s) {
            let schema = serde_json::to_value(&*t.input_schema).unwrap();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} schema must be an object",
                t.name
            );
            assert!(
                schema["properties"].get("agent_id").is_some(),
                "{} must take agent_id",
                t.name
            );
            let required = schema["required"].as_array().cloned().unwrap_or_default();
            assert!(
                required.iter().any(|r| r == "agent_id"),
                "{} must REQUIRE agent_id, not merely accept it",
                t.name
            );
            assert!(
                t.description.as_ref().is_some_and(|d| !d.is_empty()),
                "{} must carry a description — it is what the model routes on",
                t.name
            );
        }
        s.mem.close().await.expect("close");
    }

    /// **F18 (P6 carryover), pinned.** No tool may accept a client timestamp:
    /// `derive`/`record_action`/`demote` take their logical time from the
    /// interaction node, so a client-supplied one propagates to every concept
    /// and edge beneath it and backdating by 61s neuters the whole
    /// `canonization_edge_min_age` inflation guard.
    ///
    /// This asserts on the *published schema*, so it fails for a future agent
    /// who adds a timestamp field to any params struct.
    #[tokio::test]
    async fn f18_no_tool_schema_accepts_a_client_timestamp() {
        let s = server("mcp-f18").await;
        const BANNED: &[&str] = &[
            "timestamp",
            "created_at",
            "createdat",
            "now",
            "time",
            "when",
            "date",
            "occurred_at",
            "logical_time",
        ];
        for t in tools(&s) {
            let schema = serde_json::to_value(&*t.input_schema).unwrap();
            let props = schema["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for key in props.keys() {
                let k = key.to_lowercase();
                assert!(
                    !BANNED.contains(&k.as_str()),
                    "F18: tool {} accepts '{}' — timestamps are stamped server-side and no \
                     tool may take one from the client",
                    t.name,
                    key
                );
            }
        }
        s.mem.close().await.expect("close");
    }

    /// Drive a tool by its published name, from the JSON a client would send.
    ///
    /// This deserializes through the real `Parameters<T>` types, so wire-shape
    /// bugs are caught here. It deliberately does **not** go through
    /// `ToolRouter::call`: building a `RequestContext` needs a live `Peer`, and
    /// the protocol path (handshake, `tools/list`, `tools/call` dispatch) is
    /// covered end-to-end by the real Claude Code client run captured in
    /// `dev-diary/evidence/t8.2-mcp-client/`, which is stronger evidence than a
    /// hand-built context would be.
    async fn call(s: &LamboServer, name: &str, args: serde_json::Value) -> CallToolResult {
        fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
            Parameters(serde_json::from_value(v).expect("tool params deserialize"))
        }
        match name {
            "lambo_recall" => s.lambo_recall(parse(args)).await,
            "lambo_derive" => s.lambo_derive(parse(args)).await,
            "lambo_record_action" => s.lambo_record_action(parse(args)).await,
            "lambo_reserve" => s.lambo_reserve(parse(args)).await,
            "lambo_inspect" => s.lambo_inspect(parse(args)).await,
            "lambo_saints" => s.lambo_saints(parse(args)).await,
            "lambo_stats" => s.lambo_stats(parse(args)).await,
            other => panic!("unknown tool {other}"),
        }
    }

    /// Every name the router publishes must be drivable by `call` above —
    /// otherwise these tests could silently stop covering a renamed tool.
    #[tokio::test]
    async fn every_published_tool_name_is_exercised_by_the_test_harness() {
        let s = server("mcp-harness").await;
        for t in tools(&s) {
            assert!(
                matches!(
                    t.name.as_ref(),
                    "lambo_recall"
                        | "lambo_derive"
                        | "lambo_record_action"
                        | "lambo_reserve"
                        | "lambo_inspect"
                        | "lambo_saints"
                        | "lambo_stats"
                ),
                "published tool {} has no harness arm",
                t.name
            );
        }
        s.mem.close().await.expect("close");
    }

    #[tokio::test]
    async fn recall_through_the_router_returns_the_context_block() {
        let s = server("mcp-recall").await;
        // Write something to recall.
        let derived = call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [
                    {"content": "user schema", "concept_type": "entity"},
                    {"content": "must stay backward compatible", "concept_type": "constraint"}
                ]
            }),
        )
        .await;
        assert_eq!(derived.is_error, Some(false), "{derived:?}");

        let out = call(
            &s,
            "lambo_recall",
            serde_json::json!({"agent_id": "agent-a", "query": "update user schema"}),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{out:?}");

        // The text content is the T5.3 context block verbatim — that is the
        // artifact the calling agent reads.
        let text = match &out.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(
            text.contains("user schema"),
            "context block should name the recalled concept, got:\n{text}"
        );
        let structured = out.structured_content.expect("structured content");
        assert_eq!(structured["context"], serde_json::Value::String(text));
        assert!(structured["hits"].as_array().is_some_and(|h| !h.is_empty()));
        s.mem.close().await.expect("close");
    }

    #[tokio::test]
    async fn record_action_and_saints_and_stats_round_trip() {
        let s = server("mcp-roundtrip").await;
        let acted = call(
            &s,
            "lambo_record_action",
            serde_json::json!({
                "agent_id": "agent-a",
                "action": "created migrations/003.sql",
                "produces": ["migrations/003.sql"],
                "depends_on": ["user schema"]
            }),
        )
        .await;
        assert_eq!(acted.is_error, Some(false), "{acted:?}");

        // Nothing is canonical yet, but the tool must answer cleanly.
        let saints = call(
            &s,
            "lambo_saints",
            serde_json::json!({"agent_id": "agent-a"}),
        )
        .await;
        assert_eq!(saints.is_error, Some(false));
        assert_eq!(
            saints.structured_content.unwrap()["saints"],
            serde_json::json!([])
        );

        let stats = call(
            &s,
            "lambo_stats",
            serde_json::json!({"agent_id": "agent-a"}),
        )
        .await;
        assert_eq!(stats.is_error, Some(false));
        let st = stats.structured_content.unwrap();
        assert_eq!(st["session"], "mcp-roundtrip");
        assert!(st["node_count"].as_u64().unwrap() > 0);
        s.mem.close().await.expect("close");
    }

    #[tokio::test]
    async fn inspect_finds_a_concept_and_reports_a_miss() {
        let s = server("mcp-inspect").await;
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "auth middleware", "concept_type": "entity"}]
            }),
        )
        .await;

        let hit = call(
            &s,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "auth middleware"}),
        )
        .await;
        assert_eq!(hit.is_error, Some(false), "{hit:?}");
        let view = hit.structured_content.unwrap()["view"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(view.contains("auth middleware"), "{view}");

        let miss = call(
            &s,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "no such concept at all"}),
        )
        .await;
        assert_eq!(
            miss.is_error,
            Some(true),
            "a miss is a tool error the caller can read"
        );
        s.mem.close().await.expect("close");
    }

    #[tokio::test]
    async fn bad_parameters_are_refused_as_readable_tool_errors() {
        let s = server("mcp-badparams").await;
        for (tool, args) in [
            (
                "lambo_recall",
                serde_json::json!({"agent_id": "", "query": "x"}),
            ),
            (
                "lambo_recall",
                serde_json::json!({"agent_id": "a", "query": "  "}),
            ),
            (
                "lambo_recall",
                serde_json::json!({"agent_id": "a", "query": "x", "top_k": 10_000}),
            ),
            (
                "lambo_derive",
                serde_json::json!({"agent_id": "a", "concepts": []}),
            ),
            (
                "lambo_record_action",
                serde_json::json!({"agent_id": "a", "action": ""}),
            ),
            (
                "lambo_reserve",
                serde_json::json!({"agent_id": "a", "node_id": "not-a-uuid"}),
            ),
            (
                "lambo_inspect",
                serde_json::json!({"agent_id": "a", "focus": "x", "depth": 99}),
            ),
        ] {
            let out = call(&s, tool, args.clone()).await;
            assert_eq!(
                out.is_error,
                Some(true),
                "{tool} should refuse {args}, got {out:?}"
            );
        }
        s.mem.close().await.expect("close");
    }

    /// The attribution gap is *reported*, never silent: `Memory` binds one
    /// agent, so a call from a different `agent_id` must say so.
    #[tokio::test]
    async fn a_foreign_agent_id_is_reported_not_silently_dropped() {
        let s = server("mcp-attribution").await;
        let out = call(
            &s,
            "lambo_stats",
            serde_json::json!({"agent_id": "agent-b"}),
        )
        .await;
        let warnings = out.structured_content.unwrap()["warnings"].clone();
        let joined = warnings.to_string();
        assert!(
            joined.contains("agent-b") && joined.contains("agent-a"),
            "a foreign agent_id must be reported, got {joined}"
        );
        s.mem.close().await.expect("close");
    }

    #[tokio::test]
    async fn reserve_takes_and_releases_a_soft_lock() {
        let s = server("mcp-reserve").await;
        let derived = call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "session store", "concept_type": "entity"}]
            }),
        )
        .await;
        let created = derived.structured_content.unwrap()["created"][0]
            .as_str()
            .unwrap()
            .to_string();

        let held = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-a", "node_id": created, "ttl_seconds": 30}),
        )
        .await;
        assert_eq!(held.is_error, Some(false), "{held:?}");

        let freed = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-a", "node_id": created, "release": true}),
        )
        .await;
        assert_eq!(freed.is_error, Some(false), "{freed:?}");
        s.mem.close().await.expect("close");
    }

    /// A closed session must refuse writes through the MCP surface too, as a
    /// readable tool error rather than a panic or a silent success.
    #[tokio::test]
    async fn a_closed_session_refuses_writes_through_the_tools() {
        let s = server("mcp-closed").await;
        s.mem.close().await.expect("close");
        let out = call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "too late", "concept_type": "entity"}]
            }),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{out:?}");
    }

    #[tokio::test]
    async fn get_info_advertises_tools_and_names_the_session() {
        let s = server("mcp-info").await;
        let info = s.get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised"
        );
        assert_eq!(info.server_info.name, "lambo");
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.contains("mcp-info"));
        assert!(
            instructions.contains("Never send a timestamp"),
            "instructions should tell the model not to send timestamps (F18)"
        );
        s.mem.close().await.expect("close");
    }
}
