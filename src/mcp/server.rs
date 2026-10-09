//! The seven MCP tools of `lambo serve` (spec §6.2).
//!
//! One process owns the session (spec §2.2); every tool call is a task inside
//! it. [`LamboServer`] is a cheap handle — cloning it clones an `Arc<Memory>`,
//! never a second [`Memory`] (a second one would spawn a rival task trio
//! against a divergent RAM copy of the same session).
//!
//! # F18 — flush timestamps are server-side
//!
//! `created_at` remains server-stamped for every tool call. `lambo_derive` and
//! `lambo_record_action` additionally accept an optional `event_time`: the
//! historical about-time of a fact, such as its commit or document date. It is
//! not an observed-at claim and therefore does not weaken the server's flush
//! timestamp authority. No other client-supplied time surface is accepted.
//!
//! # Error convention
//!
//! Per rmcp's own guidance: `Err(ErrorData)` is for requests the server cannot
//! route (the client renders those opaquely, so the message never reaches the
//! user); `Ok(CallToolResult::error(..))` is for "the tool ran and it did not
//! work", whose content the caller actually sees. Memory-level failures —
//! conflicts, unknown nodes, a closed session — are the latter.
//!
//! # Layout
//!
//! This file is the facade: the [`LamboServer`] handle, its constructors, the
//! per-call orchestration (`answered`: trace, panic containment, receipt
//! delivery) and the thin `#[tool]` wrappers rmcp's router macros see, kept
//! together here. Everything else is delegated:
//!
//! * `params` — the published parameter schemas and the `agent_id` door check;
//! * `response` — model-safe errors, warnings, receipts and panic containment;
//! * `trace` — the I1 ledger's per-call trace slot and recall facts;
//! * `stats` — the `lambo_stats` / I2 heartbeat payload;
//! * `tools` — the seven tool bodies, one file per tool (writes share one).

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::types::AgentId;

mod params;
mod response;
mod stats;
mod tools;
mod trace;

pub use params::{
    DeriveParams, InspectParams, RecallParams, RecordActionParams, ReserveParams, SaintsParams,
    StatsParams, WireConcept, WireConceptType, WireParentOf, WireResource,
};
use response::attach_receipts;
pub(crate) use trace::CallCredential;

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// MCP surface over one [`Memory`].
#[derive(Clone)]
pub struct LamboServer {
    mem: Arc<Memory>,
    tool_router: ToolRouter<Self>,
    /// I1 call ledger, `None` unless `serve --ledger` named a path.
    ///
    /// `None` is the whole of "off": no scope is established, so no facts are
    /// built, no timestamps are taken beyond the one `Instant` every call
    /// already affords, and `lambo_stats` emits exactly the payload it emitted
    /// before this field existed.
    ledger: Option<Arc<Ledger>>,
    /// When this process's server handle was created — the heartbeat's uptime.
    started_at: Instant,
}

impl std::fmt::Debug for LamboServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LamboServer")
            .field("session", self.mem.session())
            .field("agent", self.mem.agent())
            .field("ledger", &self.ledger.as_ref().map(|l| l.path()))
            .finish_non_exhaustive()
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
            ledger: None,
            started_at: Instant::now(),
        }
    }

    /// The same handle, recording every tool call to the I1 ledger.
    ///
    /// `serve --ledger` is the only caller. Clones share the ledger, as they
    /// share the `Memory`: the streamable-http transport clones this handle per
    /// request and all of them must append to one file.
    ///
    /// The handle kept is `ledger` scoped to this server's session
    /// ([`Ledger::for_session`]), so every call line names the session it was
    /// made against (#32 decision 15); it shares `ledger`'s file and counters.
    pub fn with_ledger(mem: Arc<Memory>, ledger: Arc<Ledger>) -> Self {
        let ledger = ledger.for_session(&mem.session().0);
        Self {
            ledger: Some(ledger),
            ..Self::new(mem)
        }
    }

    /// The session this process owns.
    pub fn memory(&self) -> &Arc<Memory> {
        &self.mem
    }

    /// The call ledger, when one is configured.
    pub fn ledger(&self) -> Option<&Arc<Ledger>> {
        self.ledger.as_ref()
    }

    /// Run a tool through [`LamboServer::observed`] and then piggyback this
    /// caller's settled write receipts onto the result (J3).
    ///
    /// **One wrapper, so it cannot be forgotten for one tool.** The same
    /// reasoning that keeps every `*_impl` behind
    /// [`contain_panic`](response::contain_panic) applies here: the piggyback
    /// is the delivery channel for outcomes that no longer arrive on the
    /// write's own response, and a tool that skipped it would be a tool after
    /// which an agent silently never hears about its writes. It deliberately
    /// does not live inside `observed`, which returns early when no ledger is
    /// attached — that would have made receipt delivery depend on `--ledger`.
    ///
    /// Every tool carries it, `lambo_derive` included: a derive whose own ack
    /// is a receipt is exactly the call after which an *earlier* write is most
    /// likely to have settled.
    async fn answered(
        &self,
        tool: &'static str,
        agent_id: String,
        fut: impl Future<Output = CallToolResult>,
    ) -> CallToolResult {
        let mut out = self.observed(tool, self.ledger_agent(&agent_id), fut).await;
        // Only for an id the door would accept. A refused id owns no receipts
        // by construction (nothing could have been written under it), so this
        // is about not doing a lookup on an unvalidated string rather than
        // about hiding anything.
        if self.check_agent_id(&agent_id).is_ok() {
            let acting = AgentId::new(&agent_id);
            let (taken, remaining) = self.mem.pipeline().take_piggyback(&acting);
            attach_receipts(&mut out, &taken, remaining);
        }
        out
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
        let agent_id = p.agent_id.clone();
        self.answered("lambo_recall", agent_id, self.recall_impl(p))
            .await
    }

    /// Derive concepts from a fresh interaction (spec §7).
    ///
    /// The interaction's `created_at` is stamped server-side (F18). Callers may
    /// optionally supply the evidence's historical `event_time`.
    #[tool(
        name = "lambo_derive",
        description = "Derive concepts from the current interaction into session memory. \
                       created_at is stamped server-side. event_time is optional RFC3339 \
                       historical about-time, such as a commit or document date; omit it for \
                       a live fact, which is about now. Returns as soon \
                       as the input is validated and ordered — the write is applied in the \
                       background and the ack carries a receipt id. The outcome is \
                       piggybacked on your next tool response; to wait for it, call \
                       lambo_stats with that receipt and a wait_ms."
    )]
    async fn lambo_derive(&self, Parameters(p): Parameters<DeriveParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_derive", agent_id, self.derive_impl(p))
            .await
    }

    /// Record an agent action (spec §7) — a `Resource` concept plus `Causal` /
    /// `Dependency` edges, on a fresh server-stamped interaction (F18).
    #[tool(
        name = "lambo_record_action",
        description = "Record an action the agent took, with what it produces, modifies and \
                       depends on. created_at is stamped server-side. event_time is optional \
                       RFC3339 historical about-time, such as a commit or document date; omit \
                       it for a live fact, which is about now. \
                       Returns as soon as the input is validated and ordered — the write is \
                       applied in the background and the ack carries a receipt id, resolved \
                       the same way as lambo_derive's."
    )]
    async fn lambo_record_action(
        &self,
        Parameters(p): Parameters<RecordActionParams>,
    ) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_record_action", agent_id, self.record_action_impl(p))
            .await
    }

    /// Take (or release) a soft lock on a node — spec §11.
    ///
    /// Not durable: reservations are RAM-local to this process (pinned contract
    /// S5). "No reservation" after a restart does **not** mean nobody else is
    /// working on the node.
    ///
    /// **Cooperative, and said so out loud (J1).** The lock is held under the
    /// caller-asserted `agent_id`, which nothing here authenticates — over stdio
    /// the client owns the process, over HTTP one token authenticates the server
    /// rather than each agent. So: distinct ids get distinct locks and contend
    /// honestly; callers that send the same id share one lock and can release
    /// each other's. That is the same trust level §11 soft locks always had
    /// (advisory, RAM-only, never blocking a write); what J1 removed was the
    /// blanket refusal of foreign ids, which left every client but one of a
    /// shared serve with no mutual-exclusion primitive at all.
    #[tool(
        name = "lambo_reserve",
        description = "Take a soft lock on a memory node before editing it (or release one). \
                       Reservations are advisory and do not survive a server restart. The \
                       lock is held under the agent_id you send, which is caller-asserted \
                       and unverified: a distinct id gets a distinct lock, and callers \
                       sharing an id share the lock."
    )]
    async fn lambo_reserve(&self, Parameters(p): Parameters<ReserveParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_reserve", agent_id, self.reserve_impl(p))
            .await
    }

    /// Neighbourhood around a focus concept — the read-only graph view.
    #[tool(
        name = "lambo_inspect",
        description = "Inspect the neighbourhood around a concept: its type, canonization \
                       status, blast radius and typed edges out to a depth."
    )]
    async fn lambo_inspect(&self, Parameters(p): Parameters<InspectParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_inspect", agent_id, self.inspect_impl(p))
            .await
    }

    /// The canonical ("saints") memories — spec §10.
    #[tool(
        name = "lambo_saints",
        description = "List the session's canonical memories — concepts that earned Canonical \
                       status through the audited transition path."
    )]
    async fn lambo_saints(&self, Parameters(p): Parameters<SaintsParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_saints", agent_id, self.saints_impl(p))
            .await
    }

    /// Session health — the spec §2.4 observable durability bound.
    #[tool(
        name = "lambo_stats",
        description = "Session health: flush lag, write-behind log depth, background write \
                       queue depth, node/edge/concept counts, canonization progress and \
                       degraded state. Pass a receipt from a lambo_derive or \
                       lambo_record_action ack to ask what happened to that one write, and \
                       wait_ms to wait for it to be applied first."
    )]
    async fn lambo_stats(&self, Parameters(p): Parameters<StatsParams>) -> CallToolResult {
        let agent_id = p.agent_id.clone();
        self.answered("lambo_stats", agent_id, self.stats_impl(p))
            .await
    }
}

// `router = self.tool_router` on purpose: the macro's default is
// `Self::tool_router()`, which **rebuilds the whole router — every tool's JSON
// schema included — on every `tools/list` and every `tools/call`**. Pointing it
// at the field built once in `new()` keeps per-call work to a map lookup.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for LamboServer {
    /// The macro's own dispatch, inside the call's credential scope
    /// (#32 PR 5 review I6): the configured credential the HTTP request
    /// authenticated as, read from the request parts rmcp attaches, so the
    /// call ledger can name it. Stdio, and the `default` and `local`
    /// credentials, carry none.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let caller = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<CallCredential>())
            .map(|credential| Arc::clone(&credential.0));
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        trace::with_caller(caller, self.tool_router.call(call)).await
    }

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
                 lambo_stats to look around. Every tool takes your agent_id: it is \
                 caller-asserted and unverified, so send one stable id of your own — \
                 work is recorded under it, soft locks are held under it, distinct ids \
                 get distinct locks, and callers sharing an id share locks. created_at is \
                 server-stamped: do not send a client flush timestamp. lambo_derive and \
                 lambo_record_action may take optional RFC3339 event_time for historical \
                 about-time, such as a commit or document date; omit it for a live fact, \
                 which is about now. Ordering is yours to manage, and \
                 writes are applied in the BACKGROUND: lambo_derive and \
                 lambo_record_action return once your input is validated and ordered, \
                 and their ack carries a receipt id. Their outcome arrives on your next \
                 tool response. If you need a write visible to the very next read, call \
                 lambo_stats with that receipt and a wait_ms first; otherwise carry on \
                 — writes you send one after another are applied in that order (two you \
                 fire at once have no order to keep).",
            self.mem.session().0
        ));
        info
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests;
