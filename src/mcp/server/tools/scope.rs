//! `scope` on the read tools (#33 PR 3): which session a call reads.
//!
//! `lambo_recall` and `lambo_inspect` take `scope`: this session's own
//! name, or one of its configured layers. Anything else is refused with the
//! same `bad_param`, listing only this session's scopes and never quoting
//! the value sent. A layer is authorized for the call's grant here, and
//! looked up live only when the tool is about to read it
//! ([`LamboServer::read_layer`]), so a call holds a layer's `Memory` (and so
//! delays that layer's re-attach) for as short a time as it can. The
//! residual: a re-attach waits for the old handle to go for at most
//! `PREVIOUS_HANDLE_WAIT` (30 s) before attaching anyway, so a direct read
//! that outlives that bound makes the re-attach log a second-writer error
//! for a handle that is only finishing a read. A read is bounded by the
//! recall and inspect timeouts, well inside 30 s. Writes take `scope` in
//! PR 4.
//!
//! [`LayerProbe`] (#36 PR 4) is the write side's read of the layers: a
//! write's reference that names nothing in this session is looked up in each
//! layer only to say where it is, and is refused as cross-session either way.

use rmcp::model::CallToolResult;

use crate::mcp::server::layers::{LayerHandle, LayerRead, LiveLayer};
use crate::mcp::server::params::check_size;
use crate::mcp::server::response::{bad_param, config_refusal, layer_unavailable};
use crate::mcp::server::trace::call_auth;
use crate::mcp::server::LamboServer;
use crate::surface::resolve::{resolve_reference, ForeignScopes, Resolved};

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

    /// Look layer `handle` up for a direct read now, drawing its rate
    /// token; refused by why it is not served, with `fallback` saying what
    /// the caller can do without the scope.
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

    /// The configured layers' names, nearest tier first.
    pub(super) fn layer_names(&self) -> Vec<&str> {
        self.layers()
            .handles
            .iter()
            .map(|h| h.session.as_str())
            .collect()
    }
}

/// The foreign scopes of a write to this server's session (#36 PR 4): its
/// layers, nearest tier first. A reference that names nothing in the session
/// but exactly one concept in a layer is refused as "in scope '<layer>'"
/// rather than as missing; nothing found here is ever cited (dependence is
/// per session, design-33 section 7). A layer the call may not read, or one
/// not serving now, is skipped: the reference is then refused as missing.
/// #33 PR 4's scoped writes build the same lookup over the addressed session
/// and the other layers.
pub(super) struct LayerProbe<'a> {
    pub(super) server: &'a LamboServer,
}

impl ForeignScopes for LayerProbe<'_> {
    fn holder_of(&self, reference: &str) -> Option<String> {
        let layers = self.server.layers();
        let auth = call_auth();
        for handle in &layers.handles {
            let layer = handle.session.as_str();
            if !layers.authorize(auth.as_ref(), layer) {
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
