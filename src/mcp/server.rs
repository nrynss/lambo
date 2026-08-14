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

use std::future::Future;
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
use crate::store::flush::{panic_message, CatchUnwindPoll};
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
/// Upper bound on the combined `produces` + `modifies` + `depends_on` target
/// count in a single `lambo_record_action` call (N1). Mirrors
/// [`MAX_CONCEPTS_PER_DERIVE`]: `record_action` fans each target out into a
/// concept and an edge under the graph write lock, so an unbounded list is the
/// same single-process stall vector `lambo_derive` was already guarded against.
const MAX_ACTION_TARGETS: usize = 64;
/// Upper bound on `lambo_reserve` TTL — a soft lock (spec §11), not a lease.
const MAX_RESERVE_TTL_SECS: u64 = 3600;
/// Upper bound on `lambo_inspect` depth.
const MAX_INSPECT_DEPTH: usize = 5;
/// Cap on neighbours rendered per `lambo_inspect` frontier level.
const MAX_INSPECT_NODES: usize = 200;
/// Upper bound on **every** client-supplied string this surface accepts.
///
/// Sized to match `graph::hybrid::MAX_HYBRID_CONTEXT_BYTES` so the MCP layer
/// refuses before the graph does, and applied uniformly (R1/T82-6): before this,
/// `lambo_derive` was bounded only as a side effect of the hybrid path's own
/// check while `lambo_record_action` and `lambo_recall`'s `query` took input of
/// any size, so one client could grow the process every other client shares
/// through the tool that happened to have no guard.
const MAX_CONTENT_BYTES: usize = 16_384;
/// Candidate concepts listed when `lambo_inspect`'s focus is ambiguous.
const MAX_INSPECT_CANDIDATES: usize = 10;

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct WireConcept {
    /// The concept text.
    pub content: String,
    /// One of `entity`, `logic`, `constraint`, `resource`, `observation`.
    pub concept_type: WireConceptType,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireParentOf {
    pub parent: String,
    pub child: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeriveParams {
    pub agent_id: String,
    /// Concepts to derive from this interaction.
    pub concepts: Vec<WireConcept>,
    /// Optional `(parent, child)` hierarchy pairs. Both ends resolve (and may
    /// be created) as concepts.
    pub parent_of: Option<Vec<WireParentOf>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    pub agent_id: String,
    /// Concept content (or a node UUID) to centre the neighbourhood on.
    pub focus: String,
    /// Hops out from the focus (default 2, max 5).
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaintsParams {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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

/// A short, detail-free class for a `Memory` failure (N4).
///
/// The full error can interpolate a DSN, a store URL, a file path or a driver
/// message — none of which the model needs and any of which is worth keeping
/// out of a model-facing string. Return the class; the detail is logged.
fn err_class(err: &LamboError) -> &'static str {
    match err {
        LamboError::Store(_) => "store error",
        LamboError::Embed(_) => "embedding error",
        LamboError::Config(_) => "configuration error",
        LamboError::Conflict(_) => "conflict",
        LamboError::Other(_) => "internal error",
    }
}

/// Render a `Memory` failure as a caller-visible tool error (N4).
///
/// Matches the [`contain_panic`] policy: the full detail goes to the log, the
/// client gets a class and a pointer to the log — never the raw error, which
/// can carry a store URL or driver message.
fn tool_err(what: &str, err: LamboError) -> CallToolResult {
    tracing::error!(
        tool = what,
        error = %err,
        "mcp: tool returned a Memory error — full detail logged, class returned to the caller"
    );
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{what}: {} (the detail was logged server-side)",
        err_class(&err)
    ))])
}

/// Clamp a config-derived default into the MCP-enforced range (N6).
///
/// A session config can set a `default_top_k` (etc.) wider than the MCP maximum;
/// a client that omits the knob would then inherit a value the surface refuses.
/// Clamp it into `lo..=hi` and log when that changes it, so the request
/// succeeds with a legible bound rather than failing on a value the client never
/// sent.
fn clamp_cfg_default(name: &str, value: usize, lo: usize, hi: usize) -> usize {
    let clamped = value.clamp(lo, hi);
    if clamped != value {
        tracing::warn!(
            config_key = name,
            configured = value,
            clamped_to = clamped,
            "mcp: session config default is outside the MCP bound — using the clamped value"
        );
    }
    clamped
}

/// Replace any whitespace-delimited token that looks like a URL with a
/// placeholder (N3), so a warning that surfaced a store/embedder endpoint does
/// not carry it into a model-facing string. Idempotent — a redacted token has
/// no `://` left to match.
fn redact_urls(s: &str) -> String {
    s.split(' ')
        .map(|tok| {
            if tok.contains("://") {
                "<redacted-url>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reject a parameter the server will not act on.
///
/// A bad parameter is the *client's* problem and it is worth surfacing where
/// the model can read and correct it, so this is a tool-level error rather than
/// a `-32602` the client renders opaquely.
fn bad_param(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// Validate one client string before it reaches the store: refuse it if it is
/// over [`MAX_CONTENT_BYTES`] (R1/T82-6) **or** carries a control character
/// other than tab/newline (N2).
///
/// The size cap is the single-process fairness guard. The control-character
/// check is a data-hygiene one: a NUL or other C0 control ends up verbatim in a
/// concept's `content`, its canonical key, and every downstream rendering (the
/// T5.3 context block, `lambo_inspect` output, log lines), where it can corrupt
/// terminals, truncate at the NUL, or smuggle ANSI escapes. Tab and newline are
/// the only controls a legitimate multi-line concept needs, so everything else
/// is refused here rather than sanitised silently.
fn check_size(field: &str, value: &str) -> Result<(), CallToolResult> {
    if value.len() > MAX_CONTENT_BYTES {
        return Err(bad_param(format!(
            "{field} exceeds {MAX_CONTENT_BYTES} bytes ({} given)",
            value.len()
        )));
    }
    if let Some(c) = value
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        // Name the offending codepoint, never echo the raw byte back.
        return Err(bad_param(format!(
            "{field} contains a disallowed control character (U+{:04X}); only tab and newline \
             are allowed",
            c as u32
        )));
    }
    Ok(())
}

/// Attach warnings to a result **where the model will actually read them**.
///
/// R1/T82-9: warnings used to live only in `structuredContent`, which MCP
/// clients treat as optional and commonly do not surface — so the attribution
/// warning, and `Memory::recall`'s embed-failure degradation warning, reached
/// nobody. They are now a second text block. `content[0]` is deliberately left
/// alone: for `lambo_recall` it is the T5.3 context block verbatim, and that is
/// the artifact the calling agent reads.
///
/// URLs are redacted from the model-facing text (N3): a degradation warning can
/// surface a store or embedder endpoint, which the model does not need. The raw
/// warning is logged for the operator.
fn attach_warnings(out: &mut CallToolResult, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    let mut text = String::from("warnings:");
    for w in warnings {
        tracing::debug!(warning = %w, "mcp: warning attached to a result (raw, pre-redaction)");
        text.push_str("\n- ");
        text.push_str(&redact_urls(w));
    }
    out.content.push(ContentBlock::text(text));
}

/// Run a tool body with panic containment (R1/T82-5).
///
/// The MCP boundary is fed arbitrary client input, and a panicking handler used
/// to drop the JSON-RPC response entirely: no result, no error, no
/// cancellation, and the caller blocked until its own timeout. T8.1 armours
/// every store attempt this way for the same reason; the same two helpers are
/// reused here so there is one panic-containment behaviour in the tree.
///
/// The panic detail goes to the log, never to the client — a payload can
/// interpolate anything, including a DSN.
async fn contain_panic(
    tool: &'static str,
    fut: impl Future<Output = CallToolResult>,
) -> CallToolResult {
    match CatchUnwindPoll(fut).await {
        Ok(out) => out,
        Err(payload) => {
            tracing::error!(
                tool,
                panic = %panic_message(&payload),
                "mcp: tool handler panicked — contained and reported as a tool error"
            );
            CallToolResult::error(vec![ContentBlock::text(format!(
                "{tool}: internal error (the failure was logged server-side); \
                 the call had no effect beyond anything already written"
            ))])
        }
    }
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
        check_size("agent_id", agent_id)?;
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

    /// **Fail closed** when the caller is not the agent this process writes as.
    ///
    /// R1/T82-3. For the read and write tools, the attribution gap is a
    /// mis-attribution: the work happens, under the wrong name, and a warning
    /// says so. For `lambo_reserve` it is a *false safety claim*. `graph::reserve`
    /// and `graph::release` contend on the one `AgentId` this `Memory` was built
    /// with, so through MCP one client could take a soft lock another client
    /// already held, and a third could release it — each told `isError: false`.
    /// The §11 conflict could not fire, because there was only ever one agent.
    ///
    /// Mutual exclusion that reports success without providing exclusion is
    /// worse than no mutual exclusion, so a foreign `agent_id` is refused here
    /// until `Memory` grows `reserve_as`/`release_as` (a T8.1 re-open). Refusing
    /// costs a caller a lock it never really held.
    fn require_session_agent(&self, agent_id: &str, what: &str) -> Result<(), CallToolResult> {
        let owner = self.mem.agent().0.as_str();
        if agent_id == owner {
            return Ok(());
        }
        Err(CallToolResult::error(vec![ContentBlock::text(format!(
            "lambo_reserve: refusing to {what} on behalf of '{agent_id}': this process holds \
             the session as agent '{owner}' and soft locks are taken and released under that \
             single identity, so a reservation made for you could not be told apart from \
             '{owner}'s own — and you could release a lock you do not hold. \
             NOTHING WAS RESERVED OR RELEASED. Per-call agent identity needs a Memory-level \
             agent override (T8.1 re-open; see the T8.2 Handoff Log). Until then, call \
             lambo_reserve with agent_id '{owner}', or run one serve process per agent."
        ))]))
    }
}

// Each `#[tool]` handler is a thin, panic-contained wrapper (R1/T82-5) around a
// `*_impl` body in the plain `impl` block below. Keeping the bodies out of the
// macro'd block means the containment cannot be forgotten for one tool: the
// wrapper is the only thing the router can reach.
#[tool_router]
impl LamboServer {
    /// Three-phase recall (spec §8) rendered as the T5.3 context block.
    ///
    /// The block is returned verbatim as text content — it is the artifact the
    /// calling agent is meant to read — with warnings appended as a *second*
    /// text block and the hits alongside as structured content.
    #[tool(
        name = "lambo_recall",
        description = "Recall relevant memory for a query and return the Lambo context block \
                       (canonical markers, blast-radius warnings, conflict lines)."
    )]
    async fn lambo_recall(&self, Parameters(p): Parameters<RecallParams>) -> CallToolResult {
        contain_panic("lambo_recall", self.recall_impl(p)).await
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
        contain_panic("lambo_derive", self.derive_impl(p)).await
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
        contain_panic("lambo_record_action", self.record_action_impl(p)).await
    }

    /// Take (or release) a soft lock on a node — spec §11.
    ///
    /// Not durable: reservations are RAM-local to this process (pinned contract
    /// S5). "No reservation" after a restart does **not** mean nobody else is
    /// working on the node. A call whose `agent_id` is not this process's own
    /// agent is refused outright — see [`LamboServer::require_session_agent`].
    #[tool(
        name = "lambo_reserve",
        description = "Take a soft lock on a memory node before editing it (or release one). \
                       Reservations are advisory, do not survive a server restart, and are \
                       only accepted from the agent this server session runs as."
    )]
    async fn lambo_reserve(&self, Parameters(p): Parameters<ReserveParams>) -> CallToolResult {
        contain_panic("lambo_reserve", self.reserve_impl(p)).await
    }

    /// Neighbourhood around a focus concept — the read-only graph view.
    #[tool(
        name = "lambo_inspect",
        description = "Inspect the neighbourhood around a concept: its type, canonization \
                       status, blast radius and typed edges out to a depth."
    )]
    async fn lambo_inspect(&self, Parameters(p): Parameters<InspectParams>) -> CallToolResult {
        contain_panic("lambo_inspect", self.inspect_impl(p)).await
    }

    /// The canonical ("saints") memories — spec §10.
    #[tool(
        name = "lambo_saints",
        description = "List the session's canonical memories — concepts that earned Canonical \
                       status through the audited transition path."
    )]
    async fn lambo_saints(&self, Parameters(p): Parameters<SaintsParams>) -> CallToolResult {
        contain_panic("lambo_saints", self.saints_impl(p)).await
    }

    /// Session health — the spec §2.4 observable durability bound.
    #[tool(
        name = "lambo_stats",
        description = "Session health: flush lag, write-behind log depth, node/edge/concept \
                       counts, canonization progress and degraded state."
    )]
    async fn lambo_stats(&self, Parameters(p): Parameters<StatsParams>) -> CallToolResult {
        contain_panic("lambo_stats", self.stats_impl(p)).await
    }
}

/// The tool bodies. Everything the router can reach goes through
/// [`contain_panic`] above; these are the parts that do the work.
///
/// **Read tools answer from the RAM graph even after `close()`** (R1/T82-14):
/// `lambo_stats`, `lambo_saints` and `lambo_inspect` read state that is still
/// valid — a closed session's graph does not change — while every write tool
/// and `lambo_recall` refuse. That is deliberate: an operator inspecting why a
/// close failed still needs `lambo_stats` to answer.
impl LamboServer {
    async fn recall_impl(&self, p: RecallParams) -> CallToolResult {
        let mut warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.query.trim().is_empty() {
            return bad_param("query must be a non-empty string");
        }
        if let Err(e) = check_size("query", &p.query) {
            return e;
        }
        let cfg = self.mem.config();
        // N6: when the client omits a knob it inherits the session config's
        // default, which is not bound by the MCP maxima. A config wider than the
        // MCP cap (e.g. `default_top_k` above `MAX_TOP_K`) would otherwise make
        // the tool refuse a request that named nothing wrong. Clamp the
        // config-derived default into range with a logged warning; an *explicit*
        // out-of-range value from the client is still a client error and is
        // rejected below.
        let top_k = match p.top_k {
            Some(v) => v,
            None => clamp_cfg_default("default_top_k", cfg.default_top_k, 1, MAX_TOP_K),
        };
        let max_tokens = match p.max_tokens {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_max_tokens",
                cfg.default_max_tokens,
                1,
                MAX_MAX_TOKENS,
            ),
        };
        let traversal_depth = match p.traversal_depth {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_traversal_depth",
                cfg.default_traversal_depth,
                0,
                MAX_TRAVERSAL_DEPTH,
            ),
        };
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
        // These include `Memory::recall`'s embed-failure degradation warning —
        // the signal that a recall dropped its vector leg and returned
        // keyword-only hits. `attach_warnings` is what puts it where the model
        // can see it (R1/T82-9). Redact URLs as they enter the vec (N3) so both
        // the text content and the structured `warnings` are clean; the raw
        // detail is logged for the operator.
        for w in &result.warnings {
            tracing::debug!(warning = %w, "mcp: recall degradation warning (raw, pre-redaction)");
        }
        warnings.extend(result.warnings.iter().map(|w| redact_urls(w)));

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

        // `content[0]` stays the context block verbatim; warnings follow it.
        let mut out = CallToolResult::success(vec![ContentBlock::text(result.context.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "context": result.context,
            "hits": hits,
            "warnings": warnings,
        }));
        out
    }

    async fn derive_impl(&self, p: DeriveParams) -> CallToolResult {
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
        for c in &p.concepts {
            if let Err(e) = check_size("concept.content", &c.content) {
                return e;
            }
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
        for (a, b) in &pairs {
            if let Err(e) = check_size("parent_of.parent", a) {
                return e;
            }
            if let Err(e) = check_size("parent_of.child", b) {
                return e;
            }
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
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": summary,
            "created": created,
            "matched": matched,
            "warnings": warnings,
        }));
        out
    }

    async fn record_action_impl(&self, p: RecordActionParams) -> CallToolResult {
        let warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.action.trim().is_empty() {
            return bad_param("action must be a non-empty string");
        }
        if let Err(e) = check_size("action", &p.action) {
            return e;
        }
        let produces: Vec<String> = p.produces.unwrap_or_default();
        let modifies: Vec<String> = p.modifies.unwrap_or_default();
        let depends_on: Vec<String> = p.depends_on.unwrap_or_default();
        // N1: cap the combined fan-out. Without this bound one client could hand
        // `record_action` an arbitrarily long target list and hold the single
        // process's graph write lock for as long as it takes to fan every entry
        // out into a concept and an edge — the stall vector `lambo_derive` is
        // already guarded against, on the tool that had no guard.
        let total = produces.len() + modifies.len() + depends_on.len();
        if total > MAX_ACTION_TARGETS {
            return bad_param(format!(
                "produces + modifies + depends_on must total at most {MAX_ACTION_TARGETS} \
                 entries ({total} given)"
            ));
        }
        if produces
            .iter()
            .chain(&modifies)
            .chain(&depends_on)
            .any(|s| s.trim().is_empty())
        {
            return bad_param("produces / modifies / depends_on entries must be non-empty");
        }
        for s in produces.iter().chain(&modifies).chain(&depends_on) {
            if let Err(e) = check_size("produces / modifies / depends_on entry", s) {
                return e;
            }
        }

        // N1: `Memory::record_action` is synchronous and takes the graph write
        // lock for its whole body. Called inline it occupies a Tokio *worker*
        // thread until it returns, so a burst of large calls can starve the
        // runtime of workers — including the one that would run `Memory::close`
        // on SIGTERM. `spawn_blocking` moves the work to the blocking pool
        // (`Memory` is `Arc`-shared and already `Send + Sync`, as the HTTP
        // factory and the event pump rely on), keeping the worker threads free
        // for the shutdown path.
        let mem = Arc::clone(&self.mem);
        let action_owned = p.action.clone();
        let record = tokio::task::spawn_blocking(move || {
            let produces: Vec<&str> = produces.iter().map(String::as_str).collect();
            let modifies: Vec<&str> = modifies.iter().map(String::as_str).collect();
            let depends_on: Vec<&str> = depends_on.iter().map(String::as_str).collect();
            let action = Action {
                action: action_owned.as_str(),
                produces: &produces,
                modifies: &modifies,
                depends_on: &depends_on,
            };
            mem.record_action(&action)
        })
        .await;
        let outcome = match record {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return tool_err("lambo_record_action", e),
            Err(join) => {
                // The blocking task panicked. `spawn_blocking` surfaces that as a
                // `JoinError` rather than unwinding into this task, so the outer
                // `contain_panic` never sees it — contain it here to the same
                // policy: log the detail, return a class to the client.
                tracing::error!(
                    tool = "lambo_record_action",
                    error = %join,
                    "mcp: record_action task failed — contained and reported as a tool error"
                );
                return CallToolResult::error(vec![ContentBlock::text(
                    "lambo_record_action: internal error (the failure was logged \
                     server-side); the call may not have been recorded"
                        .to_string(),
                )]);
            }
        };

        let created: Vec<String> = outcome.created.iter().map(|n| n.0.to_string()).collect();
        let summary = format!(
            "recorded action '{}': {} concept(s) created, {} edge(s) added",
            p.action,
            created.len(),
            outcome.edges
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": summary,
            "action_node": outcome.action_node.0.to_string(),
            "created": created,
            "edges": outcome.edges,
            "warnings": warnings,
        }));
        out
    }

    async fn reserve_impl(&self, p: ReserveParams) -> CallToolResult {
        let mut warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        let releasing = p.release.unwrap_or(false);
        // Fail closed before touching the graph (R1/T82-3).
        if let Err(e) = self.require_session_agent(
            &p.agent_id,
            if releasing {
                "release a soft lock"
            } else {
                "take a soft lock"
            },
        ) {
            return e;
        }
        // N5: node_id is a client string too — size- and control-checked before
        // it is parsed, so the same uniform guard covers every field.
        if let Err(e) = check_size("node_id", &p.node_id) {
            return e;
        }
        let node_id = match uuid::Uuid::parse_str(p.node_id.trim()) {
            Ok(u) => NodeId(u),
            Err(e) => return bad_param(format!("node_id must be a UUID: {e}")),
        };

        if releasing {
            return match self.mem.release(node_id) {
                Ok(()) => {
                    let msg = format!("released {}", node_id.0);
                    let mut out = CallToolResult::success(vec![ContentBlock::text(msg.clone())]);
                    attach_warnings(&mut out, &warnings);
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
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": summary,
            "node_id": node_id.0.to_string(),
            "agent_id": reservation.agent_id.0,
            "expires_at": reservation.expires_at.to_rfc3339(),
            "warnings": warnings,
        }));
        out
    }

    async fn inspect_impl(&self, p: InspectParams) -> CallToolResult {
        let mut warnings = match self.attribution(&p.agent_id) {
            Ok(w) => w,
            Err(e) => return e,
        };
        if p.focus.trim().is_empty() {
            return bad_param("focus must be a non-empty string");
        }
        if let Err(e) = check_size("focus", &p.focus) {
            return e;
        }
        let depth = p.depth.unwrap_or(2);
        if depth > MAX_INSPECT_DEPTH {
            return bad_param(format!("depth must be in 0..={MAX_INSPECT_DEPTH}"));
        }

        // One short read section, no `.await` inside (spec §6.4).
        let resolved = {
            let g = self.mem.graph().read();
            match resolve_focus(&g, p.focus.trim()) {
                Focus::Exact(id) => Ok((None, render_neighbourhood(&g, id, depth))),
                Focus::Fuzzy { id, content } => Ok((
                    Some(format!(
                        "resolved '{}' → '{}' (substring match, single candidate)",
                        p.focus.trim(),
                        content
                    )),
                    render_neighbourhood(&g, id, depth),
                )),
                other => Err(other),
            }
        };

        let (note, (text, structured)) = match resolved {
            Ok(v) => v,
            Err(Focus::Ambiguous(candidates)) => {
                // Refuse rather than pick (R1/T82-7): an arbitrary pick fed a
                // node_id the caller never named into `lambo_reserve` and into
                // edits, and the pick changed between calls.
                let mut msg = format!(
                    "lambo_inspect: '{}' matches {} concepts — name one exactly, or pass its \
                     node_id:",
                    p.focus.trim(),
                    candidates.len()
                );
                for c in candidates.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
                if candidates.len() > MAX_INSPECT_CANDIDATES {
                    msg.push_str(&format!(
                        "\n  … and {} more",
                        candidates.len() - MAX_INSPECT_CANDIDATES
                    ));
                }
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            Err(_) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "lambo_inspect: no concept matching '{}' in session '{}'",
                    p.focus,
                    self.mem.session().0
                ))]);
            }
        };

        // A fuzzy resolution is stated in the *text*, not only in a warning:
        // "focus: <something the caller did not ask for>" is exactly the line a
        // model reads straight past.
        let text = match &note {
            Some(n) => {
                warnings.push(n.clone());
                format!("{n}\n{text}")
            }
            None => text,
        };

        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "view": text,
            "focus": structured,
            "resolution": note,
            "warnings": warnings,
        }));
        out
    }

    async fn saints_impl(&self, p: SaintsParams) -> CallToolResult {
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
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": text,
            "saints": rows,
            "warnings": warnings,
        }));
        out
    }

    async fn stats_impl(&self, p: StatsParams) -> CallToolResult {
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
        attach_warnings(&mut out, &warnings);
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

/// A concept `lambo_inspect` could have meant.
#[derive(Clone, Debug)]
struct FocusCandidate {
    id: NodeId,
    content: String,
}

/// How `lambo_inspect` resolved (or refused to resolve) its `focus`.
#[derive(Debug)]
enum Focus {
    /// A node UUID, or an exact (case-insensitive) content match.
    Exact(NodeId),
    /// Exactly one substring match — usable, but the caller is told.
    Fuzzy { id: NodeId, content: String },
    /// Several substring matches; the caller must disambiguate.
    Ambiguous(Vec<FocusCandidate>),
    /// Nothing matched.
    Missing,
}

/// Resolve `lambo_inspect`'s focus **deterministically** (R1/T82-7).
///
/// `Graph::concepts()` iterates a `HashMap`, so the previous `.find(..)` over
/// it picked an arbitrary match — arbitrary across runs *and* within one run,
/// since a `HashMap` reshuffles on resize. An `inspect` for "auth" could return
/// a different concept each time, with nothing in the response saying a fuzzy
/// match had happened, and that `node_id` then flowed into `lambo_reserve` and
/// into edits. Every leg here collects and sorts by a total order, and the
/// ambiguous case refuses instead of guessing.
fn resolve_focus(g: &crate::graph::Graph, focus: &str) -> Focus {
    if let Some(id) = uuid::Uuid::parse_str(focus)
        .ok()
        .map(NodeId)
        .filter(|id| g.node(*id).is_some())
    {
        return Focus::Exact(id);
    }

    let mut exact: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| c.content.eq_ignore_ascii_case(focus))
        .map(|c| FocusCandidate {
            id: c.id,
            content: c.content.clone(),
        })
        .collect();
    if !exact.is_empty() {
        // Case-insensitive duplicates are the same concept to the caller, so
        // there is nothing to disambiguate — just pick one *stably*.
        exact.sort_by(|a, b| a.content.cmp(&b.content).then(a.id.0.cmp(&b.id.0)));
        return Focus::Exact(exact[0].id);
    }

    let needle = focus.to_lowercase();
    // N8 (deferred, intentional): this allocates a lowercased `String` per
    // concept. An allocation-free case-insensitive substring search that matches
    // `to_lowercase`'s full Unicode case-folding (not just ASCII) is fiddly and
    // easy to get subtly wrong — a worse failure than the O(n) allocation on a
    // read-only, human-triggered path. Only the fuzzy leg reaches here, and only
    // when the exact leg found nothing. Revisit if `lambo_inspect` ever becomes
    // hot: a memchr-based ASCII fast path with a Unicode fallback would keep the
    // allocation off the common case without changing match semantics.
    let mut fuzzy: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| c.content.to_lowercase().contains(&needle))
        .map(|c| FocusCandidate {
            id: c.id,
            content: c.content.clone(),
        })
        .collect();
    if fuzzy.is_empty() {
        return Focus::Missing;
    }
    // Shortest content first: the least-padded match is the closest to what was
    // asked for. Content then id break ties, so the order is total.
    fuzzy.sort_by(|a, b| {
        a.content
            .len()
            .cmp(&b.content.len())
            .then_with(|| a.content.cmp(&b.content))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    if fuzzy.len() == 1 {
        let c = fuzzy.remove(0);
        Focus::Fuzzy {
            id: c.id,
            content: c.content,
        }
    } else {
        Focus::Ambiguous(fuzzy)
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
                // Budget first, `seen` second (R1/T82-15): marking a node seen
                // and *then* discovering the budget is spent permanently
                // excludes a neighbour that was never rendered. The `break`
                // only leaves the edge loop, so the outer frontier loop went on
                // burning one neighbour per remaining node.
                if budget == 0 {
                    break;
                }
                if !seen.insert(other) {
                    continue;
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
                 a timestamp: the server stamps them. Ordering is yours to manage: a \
                 read sees a write only after that write's own tool call has returned, \
                 so sequence a lambo_derive/lambo_record_action before the \
                 lambo_recall/lambo_inspect meant to see it.",
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
            for path in schema_property_paths(&schema) {
                let leaf = path.rsplit('.').next().unwrap_or(&path).to_lowercase();
                let leaf = leaf.trim_end_matches("[]").to_string();
                assert!(
                    !BANNED.contains(&leaf.as_str()),
                    "F18: tool {} accepts '{}' — timestamps are stamped server-side and no \
                     tool may take one from the client",
                    t.name,
                    path
                );
            }
        }
        s.mem.close().await.expect("close");
    }

    /// Collect **every** property path in a published schema, following `$ref`
    /// into `$defs`, `items` into array element schemas, `additionalProperties`
    /// into map values and the `allOf`/`anyOf`/`oneOf` combinators.
    ///
    /// R1/T82-4: the F18 guard used to read only the **root** `properties` map,
    /// so a `created_at` added to `WireConcept` — which `lambo_derive` publishes
    /// through `properties.concepts.items.$ref` → `$defs.WireConcept` — passed
    /// the entire suite. Mutation-verified: adding that field now fails this
    /// test and `f18_tool_schemas_match_the_golden_property_set` below.
    fn schema_property_paths(schema: &serde_json::Value) -> Vec<String> {
        fn walk(
            node: &serde_json::Value,
            prefix: &str,
            root: &serde_json::Value,
            depth: usize,
            out: &mut Vec<String>,
        ) {
            // `$defs` are acyclic here, but a recursive wire type would be a
            // legitimate future shape — bound the walk rather than hang.
            if depth > 16 {
                return;
            }
            if let Some(r) = node.get("$ref").and_then(|v| v.as_str()) {
                if let Some(name) = r.strip_prefix("#/$defs/") {
                    if let Some(target) = root.get("$defs").and_then(|d| d.get(name)) {
                        walk(target, prefix, root, depth + 1, out);
                    }
                }
            }
            if let Some(props) = node.get("properties").and_then(|v| v.as_object()) {
                for (k, v) in props {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    out.push(path.clone());
                    walk(v, &path, root, depth + 1, out);
                }
            }
            if let Some(items) = node.get("items") {
                walk(items, &format!("{prefix}[]"), root, depth + 1, out);
            }
            if let Some(ap) = node.get("additionalProperties") {
                if ap.is_object() {
                    walk(ap, &format!("{prefix}.*"), root, depth + 1, out);
                }
            }
            for key in ["allOf", "anyOf", "oneOf"] {
                if let Some(arr) = node.get(key).and_then(|v| v.as_array()) {
                    for sub in arr {
                        walk(sub, prefix, root, depth + 1, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(schema, "", schema, 0, &mut out);
        out.sort();
        out.dedup();
        out
    }

    /// **F18 as an allowlist, not a denylist** (R1/T82-4).
    ///
    /// A denylist of nine spellings is not a statement about client-supplied
    /// logical time: `ts`, `as_of` and `client_clock_ms` all sail through one.
    /// This pins the exact set of property paths every tool publishes, so
    /// *any* new field on *any* tool — nested or not, however named — fails
    /// here and forces a human to decide whether it is a timestamp in
    /// disguise. Update it deliberately, and only after answering that.
    #[tokio::test]
    async fn f18_tool_schemas_match_the_golden_property_set() {
        let s = server("mcp-f18-golden").await;
        let golden: std::collections::BTreeMap<&str, Vec<&str>> = [
            (
                "lambo_derive",
                vec![
                    "agent_id",
                    "concepts",
                    "concepts[].concept_type",
                    "concepts[].content",
                    "parent_of",
                    "parent_of[].child",
                    "parent_of[].parent",
                ],
            ),
            ("lambo_inspect", vec!["agent_id", "depth", "focus"]),
            (
                "lambo_recall",
                vec![
                    "agent_id",
                    "max_tokens",
                    "query",
                    "top_k",
                    "traversal_depth",
                ],
            ),
            (
                "lambo_record_action",
                vec!["action", "agent_id", "depends_on", "modifies", "produces"],
            ),
            (
                "lambo_reserve",
                vec!["agent_id", "node_id", "release", "ttl_seconds"],
            ),
            ("lambo_saints", vec!["agent_id"]),
            ("lambo_stats", vec!["agent_id"]),
        ]
        .into_iter()
        .collect();

        for t in tools(&s) {
            let schema = serde_json::to_value(&*t.input_schema).unwrap();
            let found = schema_property_paths(&schema);
            let expected = golden
                .get(t.name.as_ref())
                .unwrap_or_else(|| panic!("no golden property set for tool {}", t.name));
            assert_eq!(
                found,
                expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "tool {} publishes a different property set than the golden one. If the \
                 change is intended, confirm no new field carries client-supplied logical \
                 time (F18) and then update the golden set.",
                t.name
            );
        }
        s.mem.close().await.expect("close");
    }

    /// The walker must actually descend — a guard that only ever sees the root
    /// is the bug R1/T82-4 found, and it looks identical from the outside.
    #[test]
    fn the_schema_walker_reaches_nested_and_referenced_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "concepts": { "type": "array", "items": { "$ref": "#/$defs/Wire" } },
                "bag": { "additionalProperties": { "properties": { "deep": {} } } }
            },
            "$defs": { "Wire": { "properties": { "created_at": {}, "content": {} } } }
        });
        let paths = schema_property_paths(&schema);
        assert!(
            paths.contains(&"concepts[].created_at".to_string()),
            "a $ref'd nested property must be reachable, got {paths:?}"
        );
        assert!(paths.contains(&"bag.*.deep".to_string()), "{paths:?}");
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

    /// **N1 pinned.** `lambo_record_action` refuses a target list whose combined
    /// `produces` + `modifies` + `depends_on` count exceeds `MAX_ACTION_TARGETS`,
    /// and accepts one exactly at the cap — so the bound is a real cap, not an
    /// off-by-one that never trips.
    #[tokio::test]
    async fn record_action_caps_the_combined_target_count() {
        let s = server("mcp-action-cap").await;

        // One over the cap, split across all three lists, must be refused.
        let produces: Vec<String> = (0..MAX_ACTION_TARGETS).map(|i| format!("p{i}")).collect();
        let over = call(
            &s,
            "lambo_record_action",
            serde_json::json!({
                "agent_id": "agent-a",
                "action": "touch everything",
                "produces": produces,
                "modifies": ["m0"],
            }),
        )
        .await;
        assert_eq!(
            over.is_error,
            Some(true),
            "a target list over the cap must be refused: {over:?}"
        );
        let text = match &over.content[0] {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(
            text.contains(&MAX_ACTION_TARGETS.to_string()),
            "the refusal must name the cap, got: {text}"
        );

        // Exactly at the cap is accepted.
        let at_cap: Vec<String> = (0..MAX_ACTION_TARGETS).map(|i| format!("q{i}")).collect();
        let ok = call(
            &s,
            "lambo_record_action",
            serde_json::json!({
                "agent_id": "agent-a",
                "action": "touch exactly the cap",
                "produces": at_cap,
            }),
        )
        .await;
        assert_eq!(
            ok.is_error,
            Some(false),
            "a target list exactly at the cap must be accepted: {ok:?}"
        );
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

    /// Pull the concatenated text content out of a result — what an MCP client
    /// actually feeds the model.
    fn text_of(out: &CallToolResult) -> String {
        out.content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **R1/T82-3 pinned.** The reviewer's three-agent reproduction: `agent-b`
    /// must not be able to reserve a node `agent-a` holds, and `agent-c` must
    /// not be able to release it — and neither may be told it worked.
    #[tokio::test]
    async fn reserve_and_release_fail_closed_on_a_foreign_agent_id() {
        let s = server("mcp-reserve-foreign").await;
        let derived = call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "shared config", "concept_type": "entity"}]
            }),
        )
        .await;
        let node = derived.structured_content.unwrap()["created"][0]
            .as_str()
            .unwrap()
            .to_string();

        let a = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-a", "node_id": node, "ttl_seconds": 60}),
        )
        .await;
        assert_eq!(
            a.is_error,
            Some(false),
            "the session's own agent may reserve"
        );

        let b = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-b", "node_id": node, "ttl_seconds": 60}),
        )
        .await;
        assert_eq!(
            b.is_error,
            Some(true),
            "a foreign agent_id must NOT be told it took a lock it does not hold: {b:?}"
        );
        assert!(
            text_of(&b).contains("NOTHING WAS RESERVED"),
            "the refusal must say plainly that no lock was taken: {}",
            text_of(&b)
        );

        let c = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-c", "node_id": node, "release": true}),
        )
        .await;
        assert_eq!(
            c.is_error,
            Some(true),
            "a foreign agent_id must NOT be able to release someone else's lock: {c:?}"
        );

        // And the original lock is still there to be released by its owner.
        assert!(
            s.mem
                .graph()
                .read()
                .reservation(NodeId(node.parse().unwrap()))
                .is_some(),
            "agent-a's reservation must have survived both refusals"
        );
        let freed = call(
            &s,
            "lambo_reserve",
            serde_json::json!({"agent_id": "agent-a", "node_id": node, "release": true}),
        )
        .await;
        assert_eq!(freed.is_error, Some(false), "{freed:?}");
        s.mem.close().await.expect("close");
    }

    /// **R1/T82-9 pinned.** `structuredContent` is optional and commonly not
    /// surfaced; a warning only ever written there is a warning nobody reads.
    #[tokio::test]
    async fn warnings_reach_the_text_content_not_only_structured_content() {
        let s = server("mcp-warn-text").await;
        for tool in ["lambo_stats", "lambo_saints"] {
            let out = call(&s, tool, serde_json::json!({"agent_id": "agent-b"})).await;
            assert_eq!(out.is_error, Some(false));
            assert!(
                text_of(&out).contains("attribution:"),
                "{tool}: the attribution warning must be in the text content, got: {}",
                text_of(&out)
            );
        }

        // Recall keeps `content[0]` as the verbatim context block, with the
        // warnings in a block after it.
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "cache layer", "concept_type": "entity"}]
            }),
        )
        .await;
        let out = call(
            &s,
            "lambo_recall",
            serde_json::json!({"agent_id": "agent-b", "query": "cache layer"}),
        )
        .await;
        let structured = out.structured_content.clone().unwrap();
        match &out.content[0] {
            ContentBlock::Text(t) => assert_eq!(
                t.text, structured["context"],
                "content[0] must stay the context block verbatim"
            ),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(
            text_of(&out).contains("attribution:"),
            "the warning must still reach the text: {}",
            text_of(&out)
        );
        s.mem.close().await.expect("close");
    }

    /// **R1/T82-6 pinned.** Every client string is bounded, not just the ones
    /// the hybrid derive path happened to guard.
    #[tokio::test]
    async fn oversized_client_strings_are_refused_by_every_tool() {
        let s = server("mcp-oversized").await;
        let big = "A".repeat(MAX_CONTENT_BYTES + 1);
        for (tool, args) in [
            (
                "lambo_record_action",
                serde_json::json!({"agent_id": "agent-a", "action": big}),
            ),
            (
                "lambo_record_action",
                serde_json::json!({"agent_id": "agent-a", "action": "ok", "produces": [big]}),
            ),
            (
                "lambo_recall",
                serde_json::json!({"agent_id": "agent-a", "query": big}),
            ),
            (
                "lambo_derive",
                serde_json::json!({
                    "agent_id": "agent-a",
                    "concepts": [{"content": big, "concept_type": "entity"}]
                }),
            ),
            (
                "lambo_inspect",
                serde_json::json!({"agent_id": "agent-a", "focus": big}),
            ),
            ("lambo_saints", serde_json::json!({"agent_id": big})),
        ] {
            let out = call(&s, tool, args).await;
            assert_eq!(
                out.is_error,
                Some(true),
                "{tool} must refuse a string over {MAX_CONTENT_BYTES} bytes"
            );
            assert!(
                text_of(&out).contains("exceeds"),
                "{tool}: the refusal must say what was wrong, got {}",
                text_of(&out)
            );
        }
        // The graph must be untouched by all of that.
        let stats = call(
            &s,
            "lambo_stats",
            serde_json::json!({"agent_id": "agent-a"}),
        )
        .await;
        assert_eq!(
            stats.structured_content.unwrap()["concept_count"]
                .as_u64()
                .unwrap(),
            0,
            "a refused oversized write must not have reached the graph"
        );
        s.mem.close().await.expect("close");
    }

    /// **N3/N4 pinned.** Model-facing errors and warnings carry a class or a
    /// redaction, never a raw store URL or driver message.
    #[test]
    fn urls_and_raw_error_detail_are_kept_out_of_model_facing_text() {
        // N4: the tool error is a class plus a log pointer, not the raw error
        // (which here carries a DSN).
        let err = tool_err(
            "lambo_recall",
            LamboError::Store(crate::types::StoreError::Backend(
                "connect postgres://user:pw@db.internal:26257/lambo failed".into(),
            )),
        );
        let text = text_of(&err);
        assert!(text.contains("store error"), "must name the class: {text}");
        assert!(
            !text.contains("postgres://") && !text.contains("db.internal"),
            "the raw error / endpoint must not reach the client: {text}"
        );

        // N3: a warning that surfaced an endpoint is redacted.
        let redacted = redact_urls("embedder http://embed.internal:8080/v1 is down; keyword-only");
        assert!(
            !redacted.contains("http://") && !redacted.contains("embed.internal"),
            "the URL must be redacted: {redacted}"
        );
        assert!(
            redacted.contains("<redacted-url>") && redacted.contains("keyword-only"),
            "redaction keeps the rest of the message: {redacted}"
        );
        // Idempotent.
        assert_eq!(redact_urls(&redacted), redacted);
    }

    /// **N2 pinned.** A NUL (or any C0 control other than tab/newline) in a
    /// client string is refused at the MCP boundary, so it can never reach a
    /// concept's content, its canonical key, or a rendered context block — while
    /// a genuinely multi-line concept (tab + newline) is still accepted.
    #[tokio::test]
    async fn control_characters_are_refused_but_tab_and_newline_are_allowed() {
        let s = server("mcp-control-chars").await;
        for (label, bad) in [
            ("nul", "user\u{0}schema"),
            ("bell", "user\u{7}schema"),
            ("escape", "user\u{1b}[31mschema"),
        ] {
            let out = call(
                &s,
                "lambo_derive",
                serde_json::json!({
                    "agent_id": "agent-a",
                    "concepts": [{"content": bad, "concept_type": "entity"}]
                }),
            )
            .await;
            assert_eq!(
                out.is_error,
                Some(true),
                "{label}: a control character must be refused"
            );
            assert!(
                text_of(&out).contains("control character"),
                "{label}: the refusal must name the reason, got {}",
                text_of(&out)
            );
        }

        // Tab and newline are legitimate in a multi-line concept.
        let ok = call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "line one\n\tline two", "concept_type": "entity"}]
            }),
        )
        .await;
        assert_eq!(
            ok.is_error,
            Some(false),
            "tab and newline must still be accepted: {ok:?}"
        );

        // None of the refused writes touched the graph — only the valid one did.
        let stats = call(
            &s,
            "lambo_stats",
            serde_json::json!({"agent_id": "agent-a"}),
        )
        .await;
        assert_eq!(
            stats.structured_content.unwrap()["concept_count"]
                .as_u64()
                .unwrap(),
            1,
            "only the tab/newline concept should have reached the graph"
        );
        s.mem.close().await.expect("close");
    }

    /// **R1/T82-7 pinned.** An ambiguous focus is refused with the candidates
    /// named, rather than resolved to an arbitrary one of them whose `node_id`
    /// then flows into `lambo_reserve` and into edits.
    #[tokio::test]
    async fn inspect_refuses_an_ambiguous_focus_and_names_the_candidates() {
        let s = server("mcp-inspect-ambiguous").await;
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [
                    {"content": "auth middleware", "concept_type": "entity"},
                    {"content": "auth middleware rewrite", "concept_type": "entity"},
                    {"content": "legacy auth middleware shim", "concept_type": "entity"}
                ]
            }),
        )
        .await;

        let out = call(
            &s,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "auth"}),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{out:?}");
        let text = text_of(&out);
        for expected in [
            "auth middleware",
            "auth middleware rewrite",
            "legacy auth middleware shim",
        ] {
            assert!(
                text.contains(expected),
                "candidate {expected} missing: {text}"
            );
        }

        // An exact name still resolves, and says nothing about resolution.
        let exact = call(
            &s,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "auth middleware"}),
        )
        .await;
        assert_eq!(exact.is_error, Some(false), "{exact:?}");
        assert!(
            !text_of(&exact).contains("resolved '"),
            "{}",
            text_of(&exact)
        );
        s.mem.close().await.expect("close");
    }

    /// A single substring match is usable — but the caller is told, in the
    /// text, that it was not what they literally asked for.
    #[tokio::test]
    async fn inspect_reports_a_fuzzy_resolution_in_the_text() {
        let s = server("mcp-inspect-fuzzy").await;
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "postgres connection pool", "concept_type": "entity"}]
            }),
        )
        .await;
        let out = call(
            &s,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": "connection"}),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{out:?}");
        let text = text_of(&out);
        assert!(
            text.contains("resolved 'connection' → 'postgres connection pool'"),
            "a fuzzy match must announce itself in the text: {text}"
        );
        s.mem.close().await.expect("close");
    }

    /// Resolution must be a function of the graph's contents, not of hash
    /// iteration order: same graph, same answer, every time.
    #[tokio::test]
    async fn focus_resolution_is_deterministic() {
        let s = server("mcp-inspect-determinism").await;
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [
                    {"content": "queue worker", "concept_type": "entity"},
                    {"content": "queue worker retry", "concept_type": "entity"},
                    {"content": "dead letter queue worker", "concept_type": "entity"}
                ]
            }),
        )
        .await;
        let first = {
            let g = s.mem.graph().read();
            format!("{:?}", resolve_focus(&g, "queue"))
        };
        for _ in 0..20 {
            let g = s.mem.graph().read();
            assert_eq!(
                format!("{:?}", resolve_focus(&g, "queue")),
                first,
                "focus resolution must not depend on HashMap iteration order"
            );
        }
        s.mem.close().await.expect("close");
    }

    /// **R1/T82-5 pinned.** A panicking handler used to drop the JSON-RPC
    /// response entirely — no result, no error, no cancellation — leaving the
    /// caller blocked until its own timeout. It must become a readable tool
    /// error, and the panic detail must not cross the protocol.
    #[tokio::test]
    async fn a_panicking_tool_body_is_contained_as_a_tool_error() {
        let out = contain_panic("lambo_stats", async {
            panic!("MUTATION-PANIC internal detail: dsn=postgres://user:SECRET@host/db");
        })
        .await;
        assert_eq!(out.is_error, Some(true), "{out:?}");
        let text = text_of(&out);
        assert!(text.contains("internal error"), "{text}");
        assert!(
            !text.contains("SECRET") && !text.contains("dsn="),
            "the panic payload must not cross the protocol to the client: {text}"
        );
    }

    /// A tool body that does not panic must pass its result through untouched.
    #[tokio::test]
    async fn containment_does_not_disturb_a_normal_result() {
        let out = contain_panic("lambo_stats", async {
            CallToolResult::success(vec![ContentBlock::text("fine")])
        })
        .await;
        assert_eq!(out.is_error, Some(false));
        assert_eq!(text_of(&out), "fine");
    }

    /// **R1/T82-11 pinned.** An unknown field is refused, not silently
    /// discarded — including a `created_at` on a *nested* wire type. A client
    /// that believes it backdated an interaction must be told it did not.
    #[test]
    fn unknown_fields_are_refused_by_every_params_struct() {
        let ts = "1999-01-01T00:00:00Z";
        assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
            "agent_id": "a", "concepts": [], "created_at": ts
        }))
        .is_err());
        assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
            "agent_id": "a",
            "concepts": [{"content": "x", "concept_type": "entity", "created_at": ts}]
        }))
        .is_err());
        assert!(serde_json::from_value::<RecallParams>(serde_json::json!({
            "agent_id": "a", "query": "x", "ts": 1
        }))
        .is_err());
        assert!(
            serde_json::from_value::<RecordActionParams>(serde_json::json!({
                "agent_id": "a", "action": "x", "as_of": ts
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<ReserveParams>(serde_json::json!({
            "agent_id": "a", "node_id": "x", "client_clock_ms": 1
        }))
        .is_err());
        // …and the legitimate shapes still parse.
        assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
            "agent_id": "a", "concepts": [{"content": "x", "concept_type": "entity"}]
        }))
        .is_ok());
    }

    /// **R1/T82-14 pinned.** The read tools answer from the RAM graph after
    /// `close()` — documented on the impl block, and now asserted, so a future
    /// change to that behaviour is a deliberate one.
    #[tokio::test]
    async fn read_tools_still_answer_after_close() {
        let s = server("mcp-closed-reads").await;
        call(
            &s,
            "lambo_derive",
            serde_json::json!({
                "agent_id": "agent-a",
                "concepts": [{"content": "before the close", "concept_type": "entity"}]
            }),
        )
        .await;
        s.mem.close().await.expect("close");

        for (tool, args) in [
            ("lambo_stats", serde_json::json!({"agent_id": "agent-a"})),
            ("lambo_saints", serde_json::json!({"agent_id": "agent-a"})),
            (
                "lambo_inspect",
                serde_json::json!({"agent_id": "agent-a", "focus": "before the close"}),
            ),
        ] {
            let out = call(&s, tool, args).await;
            assert_eq!(
                out.is_error,
                Some(false),
                "{tool} reads a closed session's RAM graph, which does not change: {out:?}"
            );
        }
        // Recall, which needs the embedder and the store, still refuses.
        let recall = call(
            &s,
            "lambo_recall",
            serde_json::json!({"agent_id": "agent-a", "query": "before the close"}),
        )
        .await;
        assert_eq!(recall.is_error, Some(true), "{recall:?}");
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
        assert!(
            instructions.contains("Ordering is yours to manage"),
            "instructions should tell the model that write-then-read ordering is its \
             responsibility (N7)"
        );
        s.mem.close().await.expect("close");
    }

    /// **N6 pinned.** A config default outside the MCP bound is clamped into
    /// range rather than making the tool refuse a request that named nothing —
    /// while an explicit out-of-range value from the client is still rejected.
    #[test]
    fn a_config_default_over_the_mcp_bound_is_clamped_not_fatal() {
        assert_eq!(
            clamp_cfg_default("default_top_k", MAX_TOP_K + 500, 1, MAX_TOP_K),
            MAX_TOP_K,
            "a config default above the cap is clamped to the cap"
        );
        assert_eq!(
            clamp_cfg_default("default_top_k", 0, 1, MAX_TOP_K),
            1,
            "a zero config default is clamped up to the floor"
        );
        assert_eq!(
            clamp_cfg_default("default_top_k", 7, 1, MAX_TOP_K),
            7,
            "an in-range config default is left untouched"
        );
    }
}
