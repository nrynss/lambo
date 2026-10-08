//! The holder's session endpoint and the proxy probe (J2): the only part of
//! `serve` that names Unix sockets.
//!
//! `crate::mcp::endpoint` (the derived address, the bind, the socket
//! identity) and `crate::mcp::proxy` (the forwarding pipe) are Unix-only by
//! nature; this module is where the serving layer reaches them. Everything
//! else in `serve` goes through here, so a platform gate (#39) is this module
//! plus those two.

use std::sync::Arc;

use rmcp::ServiceExt;

use super::roles::ENDPOINT_NOT_ACCEPTING;
use crate::mcp::endpoint::SessionEndpoint;
use crate::mcp::server::LamboServer;

#[allow(unused_imports)] // rustdoc links only
use crate::memory::Memory;

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
