//! `scope`: which session a call acts on (#33 PR 3 for the read tools,
//! PR 4 for the writes and the remaining tools).
//!
//! `lambo_recall`, `lambo_inspect`, `lambo_derive`, `lambo_record_action`,
//! `lambo_derive_image`, `lambo_reserve` and `lambo_saints` take `scope`:
//! this session's own name, or one of its configured layers. Anything else
//! is refused with the same `bad_param`, listing only this session's scopes
//! and never quoting the value sent, before anything is written (so before
//! a write's ack). A layer is authorized for the call's grant here, and
//! looked up live (drawing its own rate token) only when the tool is about
//! to act on it ([`LamboServer::read_layer`], [`LamboServer::target`]), so
//! a call holds a layer's `Memory` (and so delays that layer's re-attach)
//! for as short a time as it can. The residual: a re-attach waits for the
//! old handle to go for at most `PREVIOUS_HANDLE_WAIT` (30 s) before
//! attaching anyway, so a call that outlives that bound makes the
//! re-attach log a second-writer error for a handle that is only finishing.
//! Reads are bounded by the recall and inspect timeouts and a write by its
//! ack (validation and ordering; the apply runs on the layer's own
//! pipeline), well inside 30 s; a `lambo_stats` receipt wait holds the
//! layer's pipeline, never its `Memory`.
//!
//! **Writes to different sessions have no ordering between them.** A
//! scoped write is validated, ordered and applied by the layer's own
//! pipeline (its own lanes, receipts and epoch); the per-agent order of
//! writes holds within one session only.
//!
//! [`LayerProbe`] (#36 PR 4) is the write side's read of the other
//! sessions: a write's reference that names nothing in the session it
//! writes to is looked up in this session and the other layers only to say
//! where it is, and is refused as cross-session either way. So no edge
//! ever joins two sessions (design-33 decision 1).

use std::sync::Arc;

use rmcp::model::CallToolResult;

use crate::mcp::server::layers::{LayerDown, LayerHandle, LayerRead, LiveLayer};
use crate::mcp::server::params::check_size;
use crate::mcp::server::response::{
    bad_param, config_refusal, layer_lost, layer_unavailable, tool_err,
};
use crate::mcp::server::trace::call_auth;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;
use crate::surface::resolve::{resolve_reference, ForeignScopes, Resolved};
use crate::surface::validate::is_bare_relative_path;
use crate::types::{AgentId, LamboError};
use crate::writeq::{ReceiptAnswer, ReceiptId, WritePipeline, MAX_PIGGYBACK_RECEIPTS};

/// What a `scope` names.
pub(super) enum Scoped<'a> {
    /// This session.
    Own,
    /// One of its layers, not yet looked up.
    Layer(&'a LayerHandle),
}

impl LamboServer {
    /// Resolve `scope` for `tool`: its size, this session's name, then one
    /// of its layers, authorized for the call.
    pub(super) fn resolve_scope(
        &self,
        tool: &str,
        scope: &str,
    ) -> Result<Scoped<'_>, CallToolResult> {
        check_size("scope", scope)?;
        if scope == self.memory().session().as_str() {
            return Ok(Scoped::Own);
        }
        let Some(handle) = self.layers().handle(scope) else {
            let mut scopes = vec![self.memory().session().as_str()];
            scopes.extend(self.layer_names());
            return Err(bad_param(format!(
                "scope must name this session or one of its layers: {}",
                scopes.join(", ")
            )));
        };
        self.authorize_layer(tool, scope)?;
        Ok(Scoped::Layer(handle))
    }

    /// Defence in depth (design section 1, Auth): the call must reach
    /// `layer`. The start refused any credential that reaches this session
    /// but not one of its layers, so a refusal here is a configuration bug
    /// (or an HTTP call without a grant): answered generically, without
    /// naming the layer, and logged by name for the operator.
    pub(super) fn authorize_layer(&self, tool: &str, layer: &str) -> Result<(), CallToolResult> {
        if self.layers().authorize(call_auth().as_ref(), layer) {
            return Ok(());
        }
        tracing::error!(
            session = %self.memory().session(),
            layer = %layer,
            "mcp: a call to a layered session was refused its layer; the serve's start check \
             should have refused this configuration"
        );
        Err(config_refusal(
            tool,
            "this credential cannot read every session this call reads; ask the operator to \
             check the serve's layer and credential configuration",
        ))
    }

    /// Look layer `handle` up for a direct call now (a read, or a scoped
    /// write), drawing its rate token; refused by why it is not served,
    /// with `fallback` saying what the caller can do without the scope.
    pub(super) fn read_layer(
        &self,
        tool: &str,
        handle: &LayerHandle,
        fallback: &str,
    ) -> Result<LiveLayer, CallToolResult> {
        handle
            .live(LayerRead::Direct)
            .map_err(|down| layer_unavailable(tool, handle.session.as_str(), down, fallback))
    }

    /// The session `scope` names for a tool that acts on one session (#33
    /// PR 4: the writes, `lambo_reserve`, `lambo_saints`): this one when
    /// it is omitted or names this session, else the layer, resolved,
    /// authorized and looked up live now, drawing one token from the
    /// layer's own `per_session_rps`. Every refusal comes before the tool
    /// does anything, so before a write's ack.
    pub(super) fn target(&self, tool: &str, scope: Option<&str>) -> Result<Target, CallToolResult> {
        let handle = match scope.map(|s| self.resolve_scope(tool, s)) {
            None | Some(Ok(Scoped::Own)) => return Ok(Target { layer: None }),
            Some(Ok(Scoped::Layer(handle))) => handle,
            Some(Err(e)) => return Err(e),
        };
        let fallback = self.target_fallback();
        let live = self.read_layer(tool, handle, &fallback)?;
        Ok(Target {
            layer: Some(TargetLayer {
                name: handle.session.to_string(),
                live,
            }),
        })
    }

    /// What a caller refused its layer can do instead.
    fn target_fallback(&self) -> String {
        format!("call without scope to act on {}", self.memory().session())
    }

    /// `err` from acting on `target`. A layer that closed or lost its
    /// lease while this call held it (#33 PR 4, design section 6) is
    /// answered as a layer that stopped serving, retryable, never as the
    /// layer store's lease error; an erased one says it will not come
    /// back. Anything else is [`tool_err`].
    pub(super) fn target_err(
        &self,
        tool: &str,
        target: &Target,
        err: LamboError,
    ) -> CallToolResult {
        if let Some(layer) = &target.layer {
            let mem = &layer.live.mem;
            if mem.erased() {
                return layer_unavailable(
                    tool,
                    &layer.name,
                    LayerDown::Erased,
                    &self.target_fallback(),
                );
            }
            if mem.lease_lost() || mem.is_closed() {
                tracing::warn!(
                    session = %self.memory().session(),
                    layer = %layer.name,
                    tool,
                    "mcp: a scoped call's layer stopped serving during the call"
                );
                return layer_lost(tool, &layer.name, &self.target_fallback());
            }
        }
        tool_err(tool, err)
    }

    /// The configured layers' names, nearest tier first.
    pub(super) fn layer_names(&self) -> Vec<&str> {
        self.layers()
            .handles
            .iter()
            .map(|h| h.session.as_str())
            .collect()
    }
}

impl LamboServer {
    /// Every write pipeline this server's calls write to (#33 PR 4): this
    /// session's, then each layer's that is serving and that the call may
    /// read, nearest tier first. Looked up without a rate token
    /// ([`LayerRead::Receipts`]) and held as the pipeline only, never the
    /// layer's `Memory`, so a receipt wait (up to `RECEIPT_WAIT_MAX`) does
    /// not hold up the layer's re-attach. An unlayered session has one.
    fn pipelines(&self) -> Vec<Arc<WritePipeline>> {
        let mut out = vec![Arc::clone(self.memory().pipeline())];
        let layers = self.layers();
        if layers.is_empty() {
            return out;
        }
        let auth = call_auth();
        for handle in &layers.handles {
            if !layers.authorize(auth.as_ref(), handle.session.as_str()) {
                continue;
            }
            if let Ok(live) = handle.live(LayerRead::Receipts) {
                out.push(Arc::clone(live.mem.pipeline()));
            }
        }
        out
    }

    /// `agent`'s settled, undelivered write receipts from every pipeline
    /// it may have written to (design-33 section 3: the piggyback drains
    /// over all pipelines), at most [`MAX_PIGGYBACK_RECEIPTS`] across them
    /// and this session's first, plus how many still wait. For an
    /// unlayered session this is exactly its pipeline's `take_piggyback`.
    /// A layer not serving now keeps its receipts for a later call.
    pub(in crate::mcp::server) fn take_receipts(
        &self,
        agent: &AgentId,
    ) -> (Vec<(ReceiptId, ReceiptAnswer)>, usize) {
        let mut taken = Vec::new();
        let mut remaining = 0;
        for pipeline in self.pipelines() {
            let budget = MAX_PIGGYBACK_RECEIPTS.saturating_sub(taken.len());
            let (more, left) = pipeline.take_piggyback_at_most(agent, budget);
            taken.extend(more);
            remaining += left;
        }
        (taken, remaining)
    }

    /// The pipeline that answers for receipt `id` (design-33 section 3:
    /// `lambo_stats` routes by epoch): the one that issued it, or knows it
    /// from a previous process's durable intents. An id none knows goes to
    /// this session's, which answers `restart_lost` (or `never_issued`).
    pub(in crate::mcp::server) fn receipt_pipeline(&self, id: ReceiptId) -> Arc<WritePipeline> {
        let mut pipelines = self.pipelines();
        match pipelines.iter().position(|p| p.knows_receipt(id)) {
            Some(i) => pipelines.swap_remove(i),
            None => pipelines.swap_remove(0),
        }
    }
}

/// The session a write or single-session tool acts on (#33 PR 4), held
/// for the call.
pub(super) struct Target {
    /// `None` for this session.
    layer: Option<TargetLayer>,
}

/// A layer a call acts on: its name and its live handle (its `Memory` and
/// in-flight guard).
struct TargetLayer {
    name: String,
    live: LiveLayer,
}

impl Target {
    /// The memory the call acts on: the layer's, or `own`.
    pub(super) fn mem<'a>(&'a self, own: &'a Arc<Memory>) -> &'a Arc<Memory> {
        self.layer.as_ref().map_or(own, |l| &l.live.mem)
    }

    /// The layer's name, when the call acts on a layer.
    pub(super) fn layer(&self) -> Option<&str> {
        self.layer.as_ref().map(|l| l.name.as_str())
    }

    /// Whether a write here refuses bare relative paths (design-33 section
    /// 3): it does into a shared layer, which every project reads, so a
    /// relative path there would name a different file in each.
    ///
    /// **The seam for #129 part 2 (#33 PR 7).** PR 7 adds a client-supplied
    /// repo scope to the canonical key of path-like Resources; a call that
    /// carries one names its file unambiguously, and this is the one
    /// predicate it relaxes (every refusal goes through
    /// [`Target::check_paths`]).
    pub(super) fn refuses_relative_paths(&self) -> bool {
        self.layer.is_some()
    }

    /// Refuse a bare relative path among `entries` (of `field`) when
    /// [`Target::refuses_relative_paths`], naming the entry, the layer and
    /// what to do instead.
    pub(super) fn check_paths<'e>(
        &self,
        field: &str,
        entries: impl IntoIterator<Item = &'e str>,
    ) -> Result<(), CallToolResult> {
        let Some(layer) = self.layer().filter(|_| self.refuses_relative_paths()) else {
            return Ok(());
        };
        match entries.into_iter().find(|e| is_bare_relative_path(e)) {
            None => Ok(()),
            Some(entry) => Err(bad_param(format!(
                "{field} entry {:?} is a relative path, refused in scope \"{layer}\": {layer} \
                 is shared by every project, so a relative path there names a different file \
                 in each. Record file actions in the project (without scope), or use an \
                 absolute path",
                entry.trim()
            ))),
        }
    }

    /// The `foreign` scopes of a write here, for [`LayerProbe`].
    pub(super) fn probe<'a>(&'a self, server: &'a LamboServer) -> LayerProbe<'a> {
        LayerProbe {
            server,
            target: self.layer(),
        }
    }
}

/// The foreign scopes of a write (#36 PR 4, #33 PR 4): every session the
/// call can read except the one it writes to, nearest first. For a write
/// to this server's session, its layers; for a write scoped to a layer,
/// this session and then the other layers. A reference that names nothing
/// in the target but exactly one concept elsewhere is refused as "in scope
/// '<session>'" rather than as missing; nothing found here is ever cited
/// (dependence is per session, design-33 section 7), so no edge joins two
/// sessions. A layer the call may not read, or one not serving now, is
/// skipped: the reference is then refused as missing.
///
/// Each graph is read under its own read lock alone, as a temporary of one
/// statement: the core calls this only after dropping the target's guard,
/// so one session's lock is held at a time and none across an await.
pub(super) struct LayerProbe<'a> {
    pub(super) server: &'a LamboServer,
    /// The layer the write goes to; `None` for this server's session.
    pub(super) target: Option<&'a str>,
}

impl ForeignScopes for LayerProbe<'_> {
    fn holder_of(&self, reference: &str) -> Option<String> {
        if self.target.is_some() {
            let own = self.server.memory();
            let resolved = resolve_reference(&own.graph().read(), reference);
            if matches!(resolved, Resolved::Concept { .. }) {
                return Some(own.session().to_string());
            }
        }
        let layers = self.server.layers();
        let auth = call_auth();
        for handle in &layers.handles {
            let layer = handle.session.as_str();
            if Some(layer) == self.target || !layers.authorize(auth.as_ref(), layer) {
                continue;
            }
            let Ok(live) = handle.live(LayerRead::Probe) else {
                continue;
            };
            // The layer's read guard is a temporary of this statement: one
            // session's lock at a time, never across an await.
            let resolved = resolve_reference(&live.mem.graph().read(), reference);
            if matches!(resolved, Resolved::Concept { .. }) {
                return Some(layer.to_string());
            }
        }
        None
    }
}

/// A ledger fact set with the call's `scope` added (additive: only when
/// the call sent one, so an unscoped call's line is unchanged).
pub(super) fn with_scope(mut facts: serde_json::Value, scope: Option<&str>) -> serde_json::Value {
    if let Some(scope) = scope
        && let Some(map) = facts.as_object_mut()
    {
        map.insert("scope".into(), serde_json::json!(scope));
    }
    facts
}

/// The ack's line for a write acting on a layer (#33 PR 4); empty for this
/// session, so an unscoped ack is unchanged.
pub(super) fn scoped_ack_line(target: &Target) -> String {
    match target.layer() {
        None => String::new(),
        Some(layer) => format!(
            "\nwritten to session '{layer}'; writes to different sessions have no ordering \
             between them"
        ),
    }
}

/// The ack's structured content with the layer it acted on added (#33 PR
/// 4); unchanged for this session.
pub(super) fn with_session(mut payload: serde_json::Value, target: &Target) -> serde_json::Value {
    if let Some(layer) = target.layer()
        && let Some(map) = payload.as_object_mut()
    {
        map.insert("session".into(), serde_json::json!(layer));
    }
    payload
}
