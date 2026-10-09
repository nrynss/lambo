//! Which credential opened each MCP session of an attached session (#32
//! PR 5 review L1), recorded at the moment rmcp mints the session id.
//!
//! rmcp's MCP-session ids carry no owner, so the serve keeps the binding
//! itself ([`Openers`]) and a request naming an id another credential
//! opened is answered as an unknown id (`transport::serve_live`).
//!
//! The binding is written **inside** rmcp's call, by the session manager
//! rmcp mints through ([`AttributingSessions`]): the request runs in an
//! [`as_credential`] scope naming its credential, and `create_session`
//! records that credential against the new id in the same poll that put
//! the id in rmcp's map. There is no await between the two, so no drop of
//! the request's future (a client disconnect) can leave a minted MCP
//! session unattributed, and the id-bearing response cannot leave before
//! the binding exists.
//!
//! Why not record after `handle` returns, on a task of its own (#32 PR 5
//! review S1's first fix): that ran rmcp's whole `handle` on the task,
//! and so took away rmcp's own cancel-on-disconnect for a sessionless
//! request it answers directly (`serve_negotiated_request_directly`,
//! rmcp #857): the call ran on after its client left, holding its slot of
//! the session cap (review L1 of the second round). Recording at the mint
//! lets `handle` run inline, where dropping it cancels the request again.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
use rmcp::transport::streamable_http_server::session::{
    EventStore, EventStream, SessionId, SessionManager,
};

tokio::task_local! {
    /// The credential the request being handled arrived as: the opener of
    /// any MCP session rmcp mints while handling it.
    static OPENER: Arc<str>;
}

/// Run `call` (rmcp's handling of one request) as `credential`: an MCP
/// session it mints is recorded as opened by `credential`.
///
/// A task-local rather than an argument because rmcp's `create_session`
/// takes none: the request reaches the session manager only through the
/// task that is handling it, and rmcp awaits `create_session` inline in
/// that task (`StreamableHttpService::handle_post`).
pub(super) async fn as_credential<F: Future>(credential: &str, call: F) -> F::Output {
    OPENER.scope(Arc::from(credential), call).await
}

/// Which credential opened each MCP session, by `Mcp-Session-Id`.
///
/// rmcp's own map is the authority on which MCP sessions are live: an
/// entry here whose id rmcp has dropped (an idle timeout, a closed worker,
/// a detach) names nothing, and is pruned whenever an entry is added, so
/// the map stays bounded by the live count plus the sessions that ended
/// since the last `initialize`.
#[derive(Default)]
pub(super) struct Openers {
    owners: parking_lot::Mutex<HashMap<String, Arc<str>>>,
}

impl Openers {
    /// Did `credential` open the MCP session `mcp_id`? `false` for an id
    /// no credential opened here.
    pub(super) fn opened_by(&self, mcp_id: &str, credential: &str) -> bool {
        self.owners
            .lock()
            .get(mcp_id)
            .is_some_and(|owner| &**owner == credential)
    }

    /// How many of the MCP sessions `is_live` admits `credential` opened.
    pub(super) fn count_opened_by(
        &self,
        credential: &str,
        is_live: impl Fn(&str) -> bool,
    ) -> usize {
        self.owners
            .lock()
            .iter()
            .filter(|(id, owner)| &***owner == credential && is_live(id))
            .count()
    }

    /// Forget the MCP session `mcp_id` (its opener closed it).
    pub(super) fn forget(&self, mcp_id: &str) {
        self.owners.lock().remove(mcp_id);
    }
}

/// rmcp's [`LocalSessionManager`], with every MCP session it mints
/// recorded in [`Openers`] as opened by the credential of the request that
/// minted it (see the module docs). Everything else is the local manager's
/// own behaviour, delegated unchanged.
pub(super) struct AttributingSessions {
    inner: Arc<LocalSessionManager>,
    openers: Arc<Openers>,
}

impl AttributingSessions {
    pub(super) fn new(inner: Arc<LocalSessionManager>, openers: Arc<Openers>) -> Self {
        Self { inner, openers }
    }
}

impl SessionManager for AttributingSessions {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        // Prune first, while nothing is minted yet: an id that ends between
        // here and the insert below is pruned on the next mint.
        {
            let live = self.inner.sessions.read().await;
            self.openers
                .owners
                .lock()
                .retain(|id, _| live.contains_key(id.as_str()));
        }
        // `LocalSessionManager::create_session` puts the id in its map and
        // returns in the same poll (no await after the insert), and the
        // binding below is synchronous: rmcp's map and the binding gain the
        // id in one poll, with no cancellation point between them.
        let (id, transport) = self.inner.create_session().await?;
        match OPENER.try_with(Arc::clone) {
            Ok(opener) => {
                self.openers.owners.lock().insert(id.to_string(), opener);
            }
            // Every request reaches rmcp through `as_credential`
            // (`transport::serve_live`), so this is a wiring fault: the
            // session is reachable by no credential, and counts toward the
            // process cap only.
            Err(_) => tracing::warn!(
                "mcp http: an MCP session was minted outside a credential's request; no \
                 credential can reach it"
            ),
        }
        Ok((id, transport))
    }

    fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<ServerJsonRpcMessage, Self::Error>> + Send {
        self.inner.initialize_session(id, message)
    }

    fn has_session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        self.inner.has_session(id)
    }

    fn close_session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close_session(id)
    }

    // The three stream-returning calls hand back rmcp's own boxed
    // `EventStream`: the crate has no direct `futures` dependency to name
    // the `Stream` trait with, and a box per request stream is noise.
    #[allow(refining_impl_trait)]
    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<EventStream, Self::Error> {
        Ok(Box::pin(self.inner.create_stream(id, message).await?))
    }

    fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.accept_message(id, message)
    }

    #[allow(refining_impl_trait)]
    async fn create_standalone_stream(&self, id: &SessionId) -> Result<EventStream, Self::Error> {
        Ok(Box::pin(self.inner.create_standalone_stream(id).await?))
    }

    #[allow(refining_impl_trait)]
    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<EventStream, Self::Error> {
        Ok(Box::pin(self.inner.resume(id, last_event_id).await?))
    }

    // `restore_session` keeps the trait's default (`NotSupported`): the
    // serve configures no session store, so rmcp never asks, and a restore
    // would mint an MCP session no credential opened.

    fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.inner.event_store()
    }
}
