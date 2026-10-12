//! The shared layers a layered session's `lambo_recall` also reads (#33 PR
//! 3, design sections 1, 2.6 and 6).
//!
//! A [`LamboServer`](super::LamboServer) of a layered session holds one
//! [`LayerHandle`] per layer, nearest tier first. A handle never holds the
//! layer's `Memory`: it holds the serve's session registry **weakly** and
//! looks the layer up on every call ([`LayerHandle::live`]). So
//!
//! * a layer re-attached after a lost lease (a new `Memory` with the next
//!   fencing token) is the one the next call reads, never a stale handle
//!   (the #32 PR 4 review found one after re-election);
//! * there is no reference cycle: the registry owns each session, each
//!   session owns its server, and the server's handles own nothing back;
//! * losing a layer's lease detaches that layer alone; its consumers answer
//!   from their own session with a `layer_unavailable` line until the
//!   layer is re-elected.
//!
//! The lookup takes the layer's in-flight guard, as `call_tool` does for
//! the server's own session, so neither the idle sweeper nor a detach's
//! drain mistakes a layer read for an idle session.
//!
//! **Defence in depth** (design section 1, Auth): a serve refuses to start
//! unless every credential reaching a layered session reaches each of its
//! layers. Each layered call still authorizes the request's grant for the
//! layer ([`ServerLayers::authorize`]); a refusal there is a configuration
//! bug and is answered with a generic error. A call with no grant (the J2
//! session endpoint, whose boundary is the OS user) is allowed, as today.

use std::sync::{Arc, Weak};

use crate::mcp::serve::activity::InFlight;
use crate::mcp::SecretToken;
use crate::memory::Memory;
use crate::surface::session::{SessionAuthority, SessionGrant, SessionNeed};
use crate::types::SessionId;

/// Where a layer handle finds its layer: the serve's session registry.
pub(crate) trait LayerSource: Send + Sync {
    /// The layer `session` if it is attached and serving now, with a call
    /// in flight on it for as long as the answer is held; otherwise why
    /// not. A [`LayerRead::Direct`] read also draws one token from the
    /// layer's own request bucket (`[serve] per_session_rps`).
    fn live_layer(&self, session: &str, read: LayerRead) -> Result<LiveLayer, LayerDown>;
}

/// How a call reads a layer (#33 PR 3 review L3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayerRead {
    /// One tier of a merged recall. Draws nothing from the layer's own
    /// rate: the call was admitted by its own session's bucket and its
    /// credential's, which bound the layer reads it causes to the
    /// consumers' rates, and a flood on the layer must not silently empty
    /// every consumer's merged answer of the layer (it would degrade to
    /// `layer_unavailable` with no refusal to say why).
    Merged,
    /// A `scope` naming the layer: the call reads the layer alone, as a
    /// request to `/mcp/s/<layer>` would, so it draws the layer's token.
    Direct,
}

/// Why a layer is not served right now (#33 PR 3 review L1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayerDown {
    /// Held by another writer, being attached, detached or erased, or the
    /// serve is shutting down: retrying can help.
    Transient,
    /// Its attach failed with an error that will not clear on its own; it
    /// is not retried until the serve restarts.
    Failed,
    /// Erased: it never comes back in this serve.
    Erased,
    /// Its own request bucket is empty (a direct read only).
    RateLimited,
}

/// A layer found live, held for one call.
pub(crate) struct LiveLayer {
    /// The layer's memory.
    pub(crate) mem: Arc<Memory>,
    /// The layer's in-flight guard; `None` from a source without activity
    /// tracking.
    pub(crate) _in_flight: Option<InFlight>,
}

/// One configured layer of a layered session.
#[derive(Clone)]
pub(crate) struct LayerHandle {
    /// The layer's session.
    pub(crate) session: SessionId,
    /// Its merge weight.
    pub(crate) weight: f64,
    /// Its floor.
    pub(crate) min_similarity: f64,
    /// The registry, weakly: see the module docs.
    source: Weak<dyn LayerSource>,
}

impl LayerHandle {
    /// A handle on layer `session`, found through `source`.
    pub(crate) fn new(
        session: &str,
        weight: f64,
        min_similarity: f64,
        source: Weak<dyn LayerSource>,
    ) -> Self {
        Self {
            session: SessionId::new(session),
            weight,
            min_similarity,
            source,
        }
    }

    /// The layer, looked up now; why not when it is not serving (a gone
    /// registry is a serve shutting down: transient).
    pub(crate) fn live(&self, read: LayerRead) -> Result<LiveLayer, LayerDown> {
        match self.source.upgrade() {
            Some(source) => source.live_layer(self.session.as_str(), read),
            None => Err(LayerDown::Transient),
        }
    }
}

impl std::fmt::Debug for LayerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerHandle")
            .field("session", &self.session)
            .field("weight", &self.weight)
            .field("min_similarity", &self.min_similarity)
            .finish_non_exhaustive()
    }
}

/// A server's layer stack and the credential set its calls are checked
/// against. Empty (the default) for an unlayered session.
#[derive(Clone, Default)]
pub(crate) struct ServerLayers {
    /// Nearest tier first.
    pub(crate) handles: Vec<LayerHandle>,
    /// The serve's credential set, for the per-call check; `None` outside
    /// an HTTP serve.
    pub(crate) authority: Option<Arc<SessionAuthority<SecretToken>>>,
}

impl ServerLayers {
    /// Is any layer configured?
    pub(crate) fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// The handle of layer `session`, when it is one of this stack's.
    pub(crate) fn handle(&self, session: &str) -> Option<&LayerHandle> {
        self.handles.iter().find(|h| h.session.as_str() == session)
    }

    /// May the current call's `grant` read layer `session`? Always, without
    /// a grant (the J2 endpoint) or without a credential set (not HTTP).
    pub(crate) fn authorize(&self, grant: Option<&SessionGrant>, session: &str) -> bool {
        match (&self.authority, grant) {
            (Some(authority), Some(grant)) => authority
                .authorize(grant, session, SessionNeed::Use)
                .is_ok(),
            _ => true,
        }
    }
}

impl std::fmt::Debug for ServerLayers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerLayers")
            .field("handles", &self.handles)
            .finish_non_exhaustive()
    }
}
