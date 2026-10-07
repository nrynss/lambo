//! Shutdown: the grace window bounds the drain.

use super::*;

// ---- shutdown -------------------------------------------------------

/// The grace window bounds the **drain**, not the server.
///
/// Regression: the first cut wrapped the whole `axum::serve` future in
/// `timeout(SHUTDOWN_GRACE, ..)`, so the process served happily for five
/// seconds and then exited 0 on its own — which is how it was caught, by
/// running it. The unit tests could not see it because they call
/// `axum::serve` directly.
#[tokio::test]
async fn the_grace_window_bounds_the_drain_not_the_server() {
    let store = Arc::new(MemoryStore::new());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let grace = Duration::from_millis(40);

    let server = tokio::spawn(async move {
        serve_bounded(
            listener,
            router(state_on(store, "t85-grace")),
            std::future::pending::<()>(), // no signal, ever
            grace,
        )
        .await
    });

    // Well past the grace window with no shutdown signal: still serving.
    tokio::time::sleep(grace * 6).await;
    assert!(!server.is_finished(), "the server exited without a signal");
    let alive = request(addr, "GET", "/healthz").await;
    assert_eq!(
        alive.status, 200,
        "the grace window must bound the post-signal drain, not the server's lifetime"
    );

    server.abort();
}

#[tokio::test]
async fn a_shutdown_signal_stops_the_server_within_the_grace_window() {
    let store = Arc::new(MemoryStore::new());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind");

    let out = tokio::time::timeout(
        Duration::from_secs(5),
        serve_bounded(
            listener,
            router(state_on(store, "t85-signal")),
            std::future::ready(()), // signal already pending
            Duration::from_millis(50),
        ),
    )
    .await
    .expect("a signalled server must return, not hang")
    .expect("clean stop");
    assert!(out.starts_with(STOPPED), "{out}");
}
