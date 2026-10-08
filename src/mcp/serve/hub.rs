//! The holder's session endpoint and the proxy probe (J2): the only part of
//! `serve` that names Unix sockets.
//!
//! `crate::mcp::endpoint` (the derived address, the bind, the socket
//! identity) and `crate::mcp::proxy` (the forwarding pipe) are Unix-only by
//! nature; this module is where the serving layer reaches them. Everything
//! else in `serve` goes through here, so a platform gate (#39) is this module
//! plus those two.
//!
//! What crosses the boundary:
//!
//! * [`derive_endpoint`], the pre-lease address derivation (creates and binds nothing);
//! * [`probe_holder`], the election's "can I forward to this holder?" probe;
//! * [`bind_hub`] and [`Hub::release`], the holder's bind and its exit;
//! * the types [`SessionEndpoint`] (named by `serve_builder`, `build_memory`
//!   and `resolve_role`) and [`HubProxy`] (carried by `Role::Proxy`), which
//!   the rest of `serve` names through this module.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::roles::ENDPOINT_NOT_ACCEPTING;
use crate::mcp::endpoint::SocketIdentity;
use crate::mcp::server::LamboServer;
use crate::store::StoreConfig;

pub(super) use crate::mcp::endpoint::SessionEndpoint;
pub(super) use crate::mcp::proxy::HubProxy;

/// The session endpoint's address for this session and store (J2), or `None`
/// when there is none to be had: a store no second process can see, or an
/// unusable base directory (logged at ERROR inside
/// [`SessionEndpoint::for_store`]; it degrades, it never refuses, J2-R1-5).
///
/// Pre-lease by design: it creates nothing and binds nothing. See the call
/// site in [`serve`](super::serve) for why it sits in that group.
pub(super) fn derive_endpoint(session: &str, store: &StoreConfig) -> Option<SessionEndpoint> {
    SessionEndpoint::for_store(session, store)
}

/// How long [`Hub::release`] waits for the endpoint's live sessions to end
/// once it has told them to stop.
///
/// A stopped session cancels its rmcp service, which drains the responses of
/// calls already in flight for up to 2 s (rmcp's own bound on a cancelled
/// service) and then closes the connection. By stage 6 `Memory` is closed, so
/// those calls are refused rather than run; the drain is short in practice.
/// This is the ceiling above rmcp's 2 s; a session still running at it is
/// aborted. It sits after the lease release and the final flush, so it
/// delays process exit, never the tail's durability, and like the ledger's
/// own drain it is outside `SHUTDOWN_BUDGET`.
pub(super) const ENDPOINT_RELEASE_GRACE: Duration = Duration::from_secs(3);

/// The endpoint's live sessions: one task per accepted connection, shared
/// between the accept loop (which spawns into it) and [`Hub::release`]
/// (which ends them).
type Connections = Arc<parking_lot::Mutex<JoinSet<()>>>;

/// A holder's session endpoint once [`bind_hub`] has run: the accept loop, if
/// the bind succeeded, its sessions, and the identity of the socket file it
/// created.
pub(super) struct Hub {
    accept_loop: Option<JoinHandle<()>>,
    /// Every session the accept loop started. Tracked, not detached, so
    /// [`Hub::release`] can end them at stage 6 instead of leaving them to the
    /// runtime's drop (#28 review L2).
    connections: Connections,
    /// Flipped to `true` by [`Hub::release`]: each session cancels its rmcp
    /// service and ends.
    stop: watch::Sender<bool>,
    /// JE2E-2: the identity of the socket file this process creates, captured
    /// the instant after the bind. It is what licenses the unlink at exit; see
    /// `SessionEndpoint::unlink_if_ours` for why the path and the lease are not
    /// licences of their own.
    bound_socket: Option<SocketIdentity>,
}

/// Bind the session endpoint and start its accept loop (J2).
///
/// Called by [`serve`](super::serve) below the arming (so a signal during it
/// still reaches `Memory::close`) and below `LamboServer`, which it needs. This
/// is also the first moment the unlink inside `bind` is licensed: the caller
/// holds the lease, so a socket file already at this path cannot belong to a
/// live holder.
///
/// **A bind failure does not stop this process serving memory**, the same
/// posture `Ledger::open` takes, for the same reason: reachability is a
/// service to *other* processes, and losing it must not cost this client its
/// memory. The consequence is stated at ERROR rather than swallowed, because
/// the lease row now advertises an address nothing is listening on: a proxy
/// that dials it fails honestly per call (the holder-unreachable path), which
/// is loud but is a real degradation, so the log line names it.
///
/// No endpoint: a store no second process can see, so there is no hub to be.
/// See `SessionEndpoint::for_store`.
pub(super) fn bind_hub(
    endpoint: Option<&SessionEndpoint>,
    server: &LamboServer,
    max_sessions: usize,
) -> Hub {
    let mut bound_socket: Option<SocketIdentity> = None;
    let connections: Connections = Arc::default();
    let (stop, stopped) = watch::channel(false);
    let accept_loop = match endpoint.map(|ep| (ep.path().display().to_string(), ep.bind())) {
        None => None,
        Some((path, Ok(listener))) => {
            bound_socket = endpoint.and_then(|ep| ep.file_identity());
            tracing::info!(
                endpoint = %path,
                "lambo serve: session endpoint bound — other clients on this machine can attach \
                 to this session through it"
            );
            Some(tokio::spawn(serve_endpoint(
                listener,
                server.clone(),
                max_sessions,
                Arc::clone(&connections),
                stopped,
            )))
        }
        Some((path, Err(e))) => {
            tracing::error!(
                error = %e,
                endpoint = %path,
                "lambo serve: the session endpoint could not be bound — this process still serves \
                 its own client normally, but other clients on this machine CANNOT attach to this \
                 session and their calls will fail honestly rather than reaching memory"
            );
            None
        }
    };
    Hub {
        accept_loop,
        connections,
        stop,
        bound_socket,
    }
}

impl Hub {
    /// Stop accepting, end every endpoint session, then remove the socket file
    /// if it is still ours.
    ///
    /// Runs AFTER `close()` (the caller runs this once `run_and_close` has
    /// returned), deliberately: a proxy's in-flight call must not be cut off
    /// before the tail it may have just written is durable.
    ///
    /// In order:
    ///
    /// 1. every session is told to stop, and the accept loop is aborted and
    ///    awaited, so no session starts after this point;
    /// 2. each session cancels its rmcp service and waits for it to finish
    ///    (in-flight responses drained, connection closed). The proxy on the
    ///    other end sees its hub connection drop, exactly as it does when a
    ///    holder exits, and answers with its lost-with-the-holder error;
    /// 3. the wait is bounded by [`ENDPOINT_RELEASE_GRACE`]; a session still
    ///    running then is aborted, with a WARN naming how many.
    ///
    /// So when this returns no endpoint session holds the server, and none can
    /// append to the ledger that stage 7 drains. Before #28's remediation the
    /// sessions were detached and lived on until the runtime dropped.
    ///
    /// Then the socket file goes, so the next start does not log a
    /// stale-socket warning it did not earn.
    ///
    /// JE2E-2. The unlink is licensed by the socket's own identity, not by the
    /// path and not by the lease: this process may be a FENCED ex-holder whose
    /// successor is already listening at this address, and even a clean close
    /// released the lease a few statements ago. `unlink_if_ours` removes the
    /// file only while it is still the inode this process bound.
    pub(super) async fn release(self, endpoint: Option<&SessionEndpoint>) {
        self.stop.send_replace(true);
        if let Some(accept_loop) = self.accept_loop {
            accept_loop.abort();
            // Cancelled is the expected outcome; awaiting it only guarantees
            // the loop can no longer spawn a session.
            let _ = accept_loop.await;
        }
        let mut sessions = std::mem::take(&mut *self.connections.lock());
        let drained = tokio::time::timeout(ENDPOINT_RELEASE_GRACE, async {
            while sessions.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                left = sessions.len(),
                grace = ?ENDPOINT_RELEASE_GRACE,
                "lambo serve: endpoint sessions did not end within the release grace; aborting them"
            );
            sessions.shutdown().await;
        }
        if let Some(endpoint) = endpoint {
            endpoint.unlink_if_ours(self.bound_socket);
        }
    }
}

/// Can this process reach the holder named by `held`, right now?
///
/// `Ok(())` means yes and the caller may become a proxy. `Err(why)` carries the
/// operator-facing reason, which the election either logs while it waits or
/// folds into its refusal.
///
/// **Probe only.** The connection is dropped rather than carried in, because
/// `HubProxy::run` re-reads the row and dials whoever is current at *that*
/// moment — the wedge invariant depends on the row being the authority, not on a
/// connection taken at startup.
pub(super) async fn probe_holder(
    held: &crate::memory::LeaseHeldElsewhere,
    endpoint: &SessionEndpoint,
    our_host: &str,
) -> Result<(), String> {
    let address = crate::mcp::proxy::proxyable(&held.current, endpoint, our_host)
        .map_err(|why| why.explain())?;
    crate::mcp::proxy::dial_dir(&address)
        .map_err(|e| format!("the holder's endpoint is not safe to dial ({e})"))?;
    match crate::mcp::proxy::connect(&address).await {
        Ok(stream) => {
            drop(stream);
            Ok(())
        }
        Err(e) => Err(format!("{ENDPOINT_NOT_ACCEPTING} ({e})")),
    }
}

/// Serve the session endpoint (J2) — the hub half of multi-client survivability.
///
/// One MCP session per accepted connection, all against the **one** [`Memory`](crate::memory::Memory)
/// this process owns: `server` is cloned exactly as `serve_http`'s factory
/// clones it, so every connection shares the same graph, the same write-behind
/// log, the same single-writer lease and the same call ledger. Nothing here
/// builds a second `Memory`.
///
/// `UnixStream` is an rmcp transport without any new dependency: the
/// `transport-io` feature already in use pulls `transport-async-rw`, whose
/// `IntoTransport` covers any `AsyncRead + AsyncWrite`. The wire is the same
/// newline-delimited JSON-RPC the stdio transport speaks — which is what lets a
/// proxy be a byte pipe rather than a re-implementation of the tool surface.
///
/// Runs until aborted. Each session is spawned into `connections`, not
/// detached, so [`Hub::release`] can end it; the caller releases the hub
/// *after* [`Memory::close`](crate::memory::Memory::close), so a proxy's
/// in-flight call is not cut off before the tail it may have just written is
/// durable.
pub(super) async fn serve_endpoint(
    listener: tokio::net::UnixListener,
    server: LamboServer,
    max_sessions: usize,
    connections: Connections,
    stopped: watch::Receiver<bool>,
) {
    // The same ceiling `--max-sessions` puts on the HTTP transport, for the same
    // reason: concurrently live MCP sessions are the resource, and the endpoint
    // is another door onto it. A refused connection is closed immediately, which
    // a proxy reports to its caller as an honest failure rather than a hang.
    let permits = Arc::new(tokio::sync::Semaphore::new(max_sessions.max(1)));
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // A per-connection accept error must never take down the
                // session: this process is still serving its own client.
                tracing::warn!(error = %e, "lambo serve: session endpoint accept failed");
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!(
                max_sessions,
                "lambo serve: session endpoint at the concurrent-session ceiling — \
                 refusing a connection (raise --max-sessions if this is a real workload)"
            );
            drop(stream);
            continue;
        };
        let mut sessions = connections.lock();
        // Reap the sessions that have ended, so the set holds live ones only.
        while sessions.try_join_next().is_some() {}
        sessions.spawn(serve_connection(
            server.clone(),
            stream,
            permit,
            stopped.clone(),
        ));
    }
}

/// One endpoint session: the MCP handshake, then the session until the client
/// leaves or [`Hub::release`] stops it.
///
/// On stop, the rmcp service is cancelled and awaited rather than dropped, so
/// its loop drains the responses already in flight and closes the connection
/// before this returns (see [`ENDPOINT_RELEASE_GRACE`]).
async fn serve_connection(
    server: LamboServer,
    stream: tokio::net::UnixStream,
    permit: tokio::sync::OwnedSemaphorePermit,
    mut stopped: watch::Receiver<bool>,
) {
    let _permit = permit;
    let service = tokio::select! {
        biased;
        () = stop_requested(&mut stopped) => return,
        served = server.serve(stream) => match served {
            Ok(service) => service,
            Err(e) => {
                tracing::warn!(error = %e, "lambo serve: endpoint handshake failed");
                return;
            }
        },
    };
    let cancel = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    let ended = tokio::select! {
        ended = &mut waiting => ended,
        () = stop_requested(&mut stopped) => {
            cancel.cancel();
            waiting.await
        }
    };
    if let Err(e) = ended {
        tracing::warn!(error = %e, "lambo serve: endpoint session ended in error");
    }
}

/// Resolves once [`Hub::release`] has asked the sessions to stop. A dropped
/// sender counts as a stop: the hub that owned these sessions is gone.
async fn stop_requested(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}
