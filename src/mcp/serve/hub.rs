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

use rmcp::ServiceExt;

use super::roles::ENDPOINT_NOT_ACCEPTING;
use crate::mcp::endpoint::SocketIdentity;
use crate::mcp::server::LamboServer;
use crate::store::StoreConfig;

pub(super) use crate::mcp::endpoint::SessionEndpoint;
pub(super) use crate::mcp::proxy::HubProxy;

#[allow(unused_imports)] // rustdoc links only
use crate::memory::Memory;

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

/// A holder's session endpoint once [`bind_hub`] has run: the accept loop, if
/// the bind succeeded, and the identity of the socket file it created.
pub(super) struct Hub {
    accept_loop: Option<tokio::task::JoinHandle<()>>,
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
        bound_socket,
    }
}

impl Hub {
    /// Stop the accept loop, then remove the socket file if it is still ours.
    ///
    /// The loop is aborted AFTER `close()` (the caller runs this once
    /// `run_and_close` has returned), deliberately: a proxy's in-flight call
    /// must not be cut off before the tail it may have just written is
    /// durable. Then the socket file goes, so the next start does not log a
    /// stale-socket warning it did not earn.
    ///
    /// JE2E-2. The unlink is licensed by the socket's own identity, not by the
    /// path and not by the lease: this process may be a FENCED ex-holder whose
    /// successor is already listening at this address, and even a clean close
    /// released the lease a few statements ago. `unlink_if_ours` removes the
    /// file only while it is still the inode this process bound.
    pub(super) fn release(self, endpoint: Option<&SessionEndpoint>) {
        if let Some(accept_loop) = self.accept_loop {
            accept_loop.abort();
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
/// One MCP session per accepted connection, all against the **one** [`Memory`]
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
/// Runs until aborted. The caller aborts it alongside the heartbeat, *after*
/// [`Memory::close`], so a proxy's in-flight call is not cut off before the tail
/// it may have just written is durable.
pub(super) async fn serve_endpoint(
    listener: tokio::net::UnixListener,
    server: LamboServer,
    max_sessions: usize,
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
        let server = server.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match server.serve(stream).await {
                Ok(service) => {
                    if let Err(e) = service.waiting().await {
                        tracing::warn!(error = %e, "lambo serve: endpoint session ended in error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lambo serve: endpoint handshake failed");
                }
            }
        });
    }
}
