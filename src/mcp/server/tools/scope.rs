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
//! What a scoped call holds the layer for: a read, for the read (an
//! inspect's admission explanation includes a read of the layer's store);
//! a text write, its validation, ordering and
//! submit (the apply runs on the layer's own pipeline); an image write the
//! same, but not its image embed (up to 30 s on its own), which runs after
//! [`Target::release`] holding nothing of the layer, and the layer is
//! taken back ([`LamboServer::retake`], no second token) to submit. A
//! `lambo_stats` receipt wait holds the layer's pipeline, never its
//! `Memory`. A text derive's embed is not on the call path at all.
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
use crate::surface::resolve::{resolve_reference_scoped, ForeignScopes, Resolved};
use crate::surface::validate::is_bare_relative_path;
use crate::types::{AgentId, LamboError, SessionId};
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
        let own_is_layer = self.is_layer();
        let own = self.memory().session().to_string();
        let handle = match scope.map(|s| self.resolve_scope(tool, s)) {
            None | Some(Ok(Scoped::Own)) => {
                return Ok(Target {
                    layer: None,
                    own,
                    own_is_layer,
                })
            }
            Some(Ok(Scoped::Layer(handle))) => handle,
            Some(Err(e)) => return Err(e),
        };
        let fallback = self.target_fallback();
        let live = self.read_layer(tool, handle, &fallback)?;
        Ok(Target {
            layer: Some(TargetLayer {
                name: handle.session.to_string(),
                handle: handle.clone(),
                live,
            }),
            own,
            own_is_layer,
        })
    }

    /// Take a released layer target back (#33 PR 4 review H2) after work
    /// done holding nothing of the layer: looked up again without a second
    /// rate token ([`LayerRead::Retake`]), and refused as having stopped
    /// serving when it is down now or the handle found is closed or fenced.
    pub(super) fn retake(&self, tool: &str, released: Released) -> Result<Target, CallToolResult> {
        let fallback = self.target_fallback();
        let live = match released.handle.live(LayerRead::Retake) {
            Ok(live) => live,
            Err(LayerDown::Erased) => {
                return Err(layer_unavailable(
                    tool,
                    &released.name,
                    LayerDown::Erased,
                    &fallback,
                ))
            }
            Err(_) => return Err(layer_lost(tool, &released.name, &fallback)),
        };
        let target = Target {
            layer: Some(TargetLayer {
                name: released.name,
                handle: released.handle,
                live,
            }),
            own: self.memory().session().to_string(),
            own_is_layer: self.is_layer(),
        };
        match self.stopped_serving(tool, &target) {
            Some(refusal) => Err(refusal),
            None => Ok(target),
        }
    }

    /// What a caller refused its layer can do instead.
    fn target_fallback(&self) -> String {
        format!("call without scope to act on {}", self.memory().session())
    }

    /// The refusal for a layer target that closed or lost its lease while
    /// this call held it (#33 PR 4, design section 6), or `None` while it
    /// serves (and for this session). Asked after a write's submit too
    /// (review M3): a job a fenced layer admitted is refused at its apply,
    /// so nothing of it is applied, and its receipt would answer from a
    /// pipeline no lookup reaches once the layer is re-attached.
    pub(super) fn stopped_serving(&self, tool: &str, target: &Target) -> Option<CallToolResult> {
        let layer = target.layer.as_ref()?;
        let mem = &layer.live.mem;
        if mem.erased() {
            return Some(layer_unavailable(
                tool,
                &layer.name,
                LayerDown::Erased,
                &self.target_fallback(),
            ));
        }
        if mem.lease_lost() || mem.is_closed() {
            tracing::warn!(
                session = %self.memory().session(),
                layer = %layer.name,
                tool,
                "mcp: a scoped call's layer stopped serving during the call"
            );
            return Some(layer_lost(tool, &layer.name, &self.target_fallback()));
        }
        None
    }

    /// The re-check after a scoped write's submit (review M3, refined by
    /// the second review): `Err` with the refusal when the layer lost its
    /// lease or was erased, whose apply refuses the admitted job, so
    /// nothing is applied; `Ok(Some(warning))` when it only closed, because
    /// a clean close drains jobs already admitted, so the write may well
    /// apply and a retry could duplicate it; `Ok(None)` while it serves.
    pub(super) fn after_submit(
        &self,
        tool: &str,
        target: &Target,
    ) -> Result<Option<String>, CallToolResult> {
        let Some(layer) = target.layer.as_ref() else {
            return Ok(None);
        };
        let mem = &layer.live.mem;
        if mem.erased() || mem.lease_lost() {
            return Err(self
                .stopped_serving(tool, target)
                .expect("an erased or fenced layer stopped serving"));
        }
        if mem.is_closed() {
            tracing::warn!(
                session = %self.memory().session(),
                layer = %layer.name,
                tool,
                "mcp: a scoped write's layer closed after the write was admitted"
            );
            return Ok(Some(format!(
                "layer {} closed while this call was finishing; a clean close applies writes \
                 it had already admitted, so do not resend this one: ask lambo_stats with the \
                 receipt (it answers once the layer is serving again)",
                layer.name
            )));
        }
        Ok(None)
    }

    /// `err` from acting on `target`: [`LamboServer::stopped_serving`]'s
    /// refusal when the layer stopped serving (never the layer store's lease
    /// error), else [`tool_err`].
    pub(super) fn target_err(
        &self,
        tool: &str,
        target: &Target,
        err: LamboError,
    ) -> CallToolResult {
        self.stopped_serving(tool, target)
            .unwrap_or_else(|| tool_err(tool, err))
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

/// A write pipeline this server's calls write to, and the destination its
/// receipts for this server carry.
struct Pipe {
    pipeline: Arc<WritePipeline>,
    /// `None` for this session's own pipeline; this session's name for a
    /// layer's (review H1: a layer delivers through the session that wrote).
    deliver_to: Option<SessionId>,
}

/// Where `lambo_stats` finds a receipt (#33 PR 4).
pub(in crate::mcp::server) enum ReceiptRoute {
    /// The pipeline that answers for it, and the destination to check.
    Pipeline {
        pipeline: Arc<WritePipeline>,
        deliver_to: Option<SessionId>,
    },
    /// No serving pipeline knows it, and this layer (which the call may
    /// read) is not serving now: it may be the layer's (review M2).
    LayerDown(String),
}

impl LamboServer {
    /// Every write pipeline this server's calls write to (#33 PR 4): this
    /// session's, then each layer's that is serving and that the call may
    /// read, nearest tier first, plus the names of the readable layers not
    /// serving now. Looked up without a rate token
    /// ([`LayerRead::Receipts`]) and held as the pipeline only, never the
    /// layer's `Memory`, so a receipt wait (up to `RECEIPT_WAIT_MAX`) does
    /// not hold up the layer's re-attach. An unlayered session has one.
    fn pipelines(&self) -> (Vec<Pipe>, Vec<String>) {
        let mut out = vec![Pipe {
            pipeline: Arc::clone(self.memory().pipeline()),
            deliver_to: None,
        }];
        let mut down = Vec::new();
        let layers = self.layers();
        if layers.is_empty() {
            return (out, down);
        }
        let auth = call_auth();
        for handle in &layers.handles {
            if !layers.authorize(auth.as_ref(), handle.session.as_str()) {
                continue;
            }
            match handle.live(LayerRead::Receipts) {
                Ok(live) => out.push(Pipe {
                    pipeline: Arc::clone(live.mem.pipeline()),
                    deliver_to: Some(self.memory().session().clone()),
                }),
                Err(_) => down.push(handle.session.to_string()),
            }
        }
        (out, down)
    }

    /// `agent`'s settled, undelivered write receipts for this session from
    /// every pipeline it writes to (design-33 section 3: the piggyback
    /// drains over all pipelines), at most [`MAX_PIGGYBACK_RECEIPTS`]
    /// across them, plus how many still wait. From a layer's pipeline only
    /// receipts of writes this session sent there; from this session's own
    /// only its own writes' (so `general`'s own server never hands out a
    /// project's scoped receipts, review H1). Each pipeline with receipts
    /// waiting gets a slot first (review L3), then the budget fills in tier
    /// order. For an unlayered session this is exactly its pipeline's
    /// `take_piggyback`. A layer not serving now keeps its receipts for a
    /// later call.
    pub(in crate::mcp::server) fn take_receipts(
        &self,
        agent: &AgentId,
    ) -> (Vec<(ReceiptId, ReceiptAnswer)>, usize) {
        let (pipes, _) = self.pipelines();
        let mut taken = Vec::new();
        for pipe in &pipes {
            if taken.len() >= MAX_PIGGYBACK_RECEIPTS {
                break;
            }
            let (one, _) = pipe
                .pipeline
                .take_piggyback_at_most(agent, pipe.deliver_to.as_ref(), 1);
            taken.extend(one);
        }
        let mut remaining = 0;
        for pipe in &pipes {
            let budget = MAX_PIGGYBACK_RECEIPTS.saturating_sub(taken.len());
            let (more, left) =
                pipe.pipeline
                    .take_piggyback_at_most(agent, pipe.deliver_to.as_ref(), budget);
            taken.extend(more);
            remaining += left;
        }
        (taken, remaining)
    }

    /// Where receipt `id` is answered (design-33 section 3: `lambo_stats`
    /// routes by epoch): the pipeline that issued it, or knows it from a
    /// previous process's durable intents. When none does and a layer the
    /// call may read is not serving now, the id may be that layer's, so it
    /// is [`ReceiptRoute::LayerDown`] (retryable, review M2) rather than
    /// this session's `restart_lost`. Otherwise this session's pipeline
    /// answers (`restart_lost` or `never_issued`).
    pub(in crate::mcp::server) fn route_receipt(&self, id: ReceiptId) -> ReceiptRoute {
        let (mut pipes, down) = self.pipelines();
        if let Some(i) = pipes.iter().position(|p| p.pipeline.knows_receipt(id)) {
            let pipe = pipes.swap_remove(i);
            return ReceiptRoute::Pipeline {
                pipeline: pipe.pipeline,
                deliver_to: pipe.deliver_to,
            };
        }
        if let Some(layer) = down.into_iter().next() {
            return ReceiptRoute::LayerDown(layer);
        }
        let own = pipes.swap_remove(0);
        ReceiptRoute::Pipeline {
            pipeline: own.pipeline,
            deliver_to: own.deliver_to,
        }
    }

    /// The pipeline [`LamboServer::route_receipt`] answers `id` from, for
    /// the tests' receipt waits.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(in crate::mcp::server) fn receipt_pipeline(&self, id: ReceiptId) -> Arc<WritePipeline> {
        match self.route_receipt(id) {
            ReceiptRoute::Pipeline { pipeline, .. } => pipeline,
            ReceiptRoute::LayerDown(_) => Arc::clone(self.memory().pipeline()),
        }
    }
}

/// The session a write or single-session tool acts on (#33 PR 4), held
/// for the call.
pub(super) struct Target {
    /// `None` for this session.
    layer: Option<TargetLayer>,
    /// This session's name.
    own: String,
    /// Whether this session is itself a layer of another (review L2).
    own_is_layer: bool,
}

/// A layer a call acts on: its name, its handle and its live lookup (its
/// `Memory` and in-flight guard).
struct TargetLayer {
    name: String,
    handle: LayerHandle,
    live: LiveLayer,
}

/// A layer target let go for work that must not hold it (review H2): the
/// image embed. [`LamboServer::retake`] takes it back.
pub(super) struct Released {
    name: String,
    handle: LayerHandle,
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

    /// The session whose server delivers this call's receipts, when it is
    /// not the one written to (review H1): this session, for a write
    /// scoped to a layer.
    pub(super) fn deliver_to(&self) -> Option<SessionId> {
        self.layer.as_ref().map(|_| SessionId::new(&self.own))
    }

    /// Let the layer go, keeping what [`LamboServer::retake`] needs; `None`
    /// (and nothing to retake) for this session.
    pub(super) fn release(self) -> Option<Released> {
        self.layer.map(|l| Released {
            name: l.name,
            handle: l.handle,
        })
    }

    /// Whether a write here refuses bare relative paths (design-33 section
    /// 3): it does into a shared layer, which every project reads, so a
    /// relative path there would name a different file in each. That is a
    /// write scoped to a layer, or any write to a server whose session is
    /// itself a layer (`/mcp/s/general`, review L2).
    pub(super) fn refuses_relative_paths(&self) -> bool {
        self.layer.is_some() || self.own_is_layer
    }

    /// A relative path is exempt only when this entry is actually covered by
    /// validated repository Resource identity. The caller passes no scope for
    /// non-Resource derive entries.
    pub(super) fn path_exempt(&self, entry: &str, repo_scope: Option<&str>) -> bool {
        crate::graph::resource::scoped_resource_key(entry, repo_scope).is_some()
    }

    /// Refuse a bare relative path among `entries` (of `field`) when
    /// [`Target::refuses_relative_paths`], naming the entry, the shared
    /// session and what to do instead. Entries [`Target::path_exempt`]
    /// accepts pass.
    pub(super) fn check_paths<'e>(
        &self,
        field: &str,
        entries: impl IntoIterator<Item = &'e str>,
    ) -> Result<(), CallToolResult> {
        self.check_paths_scoped(field, entries, None)
    }

    pub(super) fn check_paths_scoped<'e>(
        &self,
        field: &str,
        entries: impl IntoIterator<Item = &'e str>,
        repo_scope: Option<&str>,
    ) -> Result<(), CallToolResult> {
        if !self.refuses_relative_paths() {
            return Ok(());
        }
        let shared = self.layer().unwrap_or(&self.own);
        match entries
            .into_iter()
            .find(|e| is_bare_relative_path(e) && !self.path_exempt(e, repo_scope))
        {
            None => Ok(()),
            Some(entry) => Err(bad_param(format!(
                "{field} entry {:?} is a relative path, refused in scope \"{shared}\": {shared} \
                 is shared by every project, so a relative path there names a different file \
                 in each. Record file actions in the project's own session, or use an \
                 absolute path or a validated repo_scope",
                entry.trim()
            ))),
        }
    }

    /// The `foreign` scopes of a write here, for [`LayerProbe`].
    pub(super) fn probe<'a>(
        &'a self,
        server: &'a LamboServer,
        repo_scope: Option<&'a str>,
    ) -> LayerProbe<'a> {
        LayerProbe {
            server,
            repo_scope,
            target: self.layer(),
        }
    }
}

/// The foreign scopes of a write (#36 PR 4, #33 PR 4): every session the
/// call can read except the one it writes to, nearest first. For a write
/// to this server's session, its layers; for a write scoped to a layer,
/// this session and then the other layers. A reference that names nothing
/// in the target but exactly one concept elsewhere is refused as "in scope
/// "<session>"" rather than as missing; nothing found here is ever cited
/// (dependence is per session, design-33 section 7), so no edge joins two
/// sessions. A layer the call may not read, or one not serving now, is
/// skipped: the reference is then refused as missing.
///
/// Each graph is read under its own read lock alone, as a temporary of one
/// statement: the core calls this only after dropping the target's guard,
/// so one session's lock is held at a time and none across an await.
pub(super) struct LayerProbe<'a> {
    repo_scope: Option<&'a str>,
    pub(super) server: &'a LamboServer,
    /// The layer the write goes to; `None` for this server's session.
    pub(super) target: Option<&'a str>,
}

impl LayerProbe<'_> {
    /// This server's own session, when it is foreign (a scoped write) and
    /// holds `reference`.
    fn own_holder(&self, reference: &str) -> Option<String> {
        self.target?;
        let own = self.server.memory();
        let resolved = resolve_reference_scoped(&own.graph().read(), reference, self.repo_scope);
        matches!(resolved, Resolved::Concept { .. }).then(|| own.session().to_string())
    }
}

impl ForeignScopes for LayerProbe<'_> {
    fn holder_of(&self, reference: &str) -> Option<String> {
        if let Some(own) = self.own_holder(reference) {
            return Some(own);
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
            let resolved =
                resolve_reference_scoped(&live.mem.graph().read(), reference, self.repo_scope);
            if matches!(resolved, Resolved::Concept { .. }) {
                return Some(layer.to_string());
            }
        }
        None
    }

    /// Review L1: for a write scoped to a layer, a `record_action`
    /// `depends_on` text that names a concept of the caller's own session
    /// is refused rather than minted in the layer (which would copy the
    /// project's concept into the shared layer). An unscoped write keeps
    /// the 0.5.0 text rule.
    fn text_holder_of(&self, reference: &str) -> Option<String> {
        self.own_holder(reference)
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

/// A write ack with `warning` (from [`LamboServer::after_submit`]) added
/// to its text and its structured `warnings`; unchanged without one.
pub(super) fn with_warning(mut out: CallToolResult, warning: Option<String>) -> CallToolResult {
    if let Some(w) = warning {
        crate::mcp::server::response::attach_warnings(&mut out, std::slice::from_ref(&w));
        if let Some(map) = out
            .structured_content
            .as_mut()
            .and_then(|v| v.as_object_mut())
        {
            map.insert("warnings".into(), serde_json::json!([w]));
        }
    }
    out
}

#[cfg(test)]
mod path_exempt_tests {
    use super::Target;

    /// #129 review L9: the exemption is the entry being a scoped path, not
    /// the call merely carrying a scope.
    #[test]
    fn path_exempt_needs_a_covered_path_not_just_a_scope() {
        let layer = Target {
            layer: None,
            own: "general".into(),
            own_is_layer: true,
        };
        let repo = Some("github.com/acme/widgets");
        assert!(layer.path_exempt("README.md", repo));
        assert!(layer.path_exempt("docs/reference/", repo));
        for not_a_path in [
            "a decision",
            "https://host/a.md",
            "git@host:a.md",
            "Node.js",
        ] {
            assert!(!layer.path_exempt(not_a_path, repo), "{not_a_path}");
        }
        assert!(!layer.path_exempt("README.md", None));
        assert!(layer
            .check_paths_scoped("produces", ["README.md"], repo)
            .is_ok());
        assert!(layer
            .check_paths_scoped("produces", ["README.md"], None)
            .is_err());
    }
}
