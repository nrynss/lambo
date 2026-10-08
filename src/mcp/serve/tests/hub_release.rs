//! #28 review L2: [`Hub::release`](crate::mcp::serve::hub::Hub::release) ends
//! the endpoint's sessions.
//!
//! Each accepted endpoint connection used to run in a detached task. Stage 6
//! aborted only the accept loop, so a proxy's session lived on, against a
//! closed `Memory`, until the runtime dropped, and could append to the ledger
//! that stage 7 drains. These pin that the release ends every session and
//! that the proxy on the other end sees its connection close.

use super::*;
use crate::embed::{Embedder, FixtureEmbedder};
use crate::mcp::serve::hub::{bind_hub, ENDPOINT_RELEASE_GRACE};
use crate::mcp::SessionEndpoint;
use crate::memory::Memory;
use crate::store::{GraphStore, MemoryStore, StoreConfig, StoreKind};
use crate::types::EmbeddingContract;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"hub-release","version":"1"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

async fn mem() -> Arc<Memory> {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let m = Memory::builder()
        .session("hub-release")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("build");
    Arc::new(m)
}

/// An endpoint address inside a short per-test scratch directory. The store
/// config only feeds the address derivation; nothing opens it.
fn endpoint(dir: &crate::test_util::ScratchDir) -> SessionEndpoint {
    let store = StoreConfig {
        kind: StoreKind::Sqlite,
        path: Some(dir.join("s.db").to_str().expect("utf-8").into()),
        ..StoreConfig::default()
    };
    SessionEndpoint::resolve_in(&dir.join("run"), "hub-release", &store).expect("endpoint fits")
}

/// A client on the endpoint with the MCP handshake done, so a session task is
/// live on the hub side.
async fn attached_client(
    endpoint: &SessionEndpoint,
) -> (
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(endpoint.path())
        .await
        .expect("dial the endpoint");
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write
        .write_all(format!("{INITIALIZE}\n").as_bytes())
        .await
        .expect("send initialize");
    let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("the hub answers initialize")
        .expect("read")
        .expect("a reply line");
    assert!(reply.contains(r#""id":1"#), "initialize reply: {reply}");
    write
        .write_all(format!("{INITIALIZED}\n").as_bytes())
        .await
        .expect("send initialized");
    (lines, write)
}

#[tokio::test]
async fn release_ends_every_endpoint_session_before_it_returns() {
    let dir = crate::test_util::ScratchDir::short("hr");
    let endpoint = endpoint(&dir);
    let mem = mem().await;
    let server = LamboServer::new(Arc::clone(&mem));
    // `mem` and `server`: the two handles this test keeps.
    let baseline = Arc::strong_count(&mem);

    let hub = bind_hub(Some(&endpoint), &server, 4);
    let (mut first, _w1) = attached_client(&endpoint).await;
    let (mut second, _w2) = attached_client(&endpoint).await;
    assert!(
        Arc::strong_count(&mem) > baseline,
        "the live sessions hold the server"
    );

    tokio::time::timeout(
        ENDPOINT_RELEASE_GRACE + Duration::from_secs(2),
        hub.release(Some(&endpoint)),
    )
    .await
    .expect("release is bounded by ENDPOINT_RELEASE_GRACE");

    // Nothing the hub started still holds the session: no endpoint session can
    // run a call, or append to the ledger, after stage 6.
    assert_eq!(
        Arc::strong_count(&mem),
        baseline,
        "an endpoint session outlived Hub::release"
    );
    // And each proxy sees its hub connection close, the signal it already
    // treats as the holder going away.
    for (n, client) in [&mut first, &mut second].into_iter().enumerate() {
        let read = tokio::time::timeout(Duration::from_secs(2), client.next_line())
            .await
            .unwrap_or_else(|_| panic!("client {n}: the connection is still open after release"));
        assert!(
            matches!(read, Ok(None) | Err(_)),
            "client {n}: expected the connection closed, got {read:?}"
        );
    }
    assert!(
        !endpoint.path().exists(),
        "the socket file survived release"
    );
    mem.close().await.expect("close");
}

/// A client that connected but never finished the handshake is ended too:
/// the stop reaches a session still inside `serve()`.
#[tokio::test]
async fn release_ends_a_session_still_in_its_handshake() {
    let dir = crate::test_util::ScratchDir::short("hh");
    let endpoint = endpoint(&dir);
    let mem = mem().await;
    let server = LamboServer::new(Arc::clone(&mem));
    let baseline = Arc::strong_count(&mem);

    let hub = bind_hub(Some(&endpoint), &server, 4);
    let mut silent = UnixStream::connect(endpoint.path()).await.expect("dial");
    // Let the accept loop take the connection and start its session.
    tokio::time::sleep(Duration::from_millis(200)).await;

    tokio::time::timeout(
        ENDPOINT_RELEASE_GRACE + Duration::from_secs(2),
        hub.release(Some(&endpoint)),
    )
    .await
    .expect("release is bounded");
    assert_eq!(
        Arc::strong_count(&mem),
        baseline,
        "a handshaking endpoint session outlived Hub::release"
    );
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::io::AsyncReadExt::read(&mut silent, &mut buf),
    )
    .await
    .expect("the connection is still open after release");
    assert!(matches!(read, Ok(0) | Err(_)), "expected EOF, got {read:?}");
    mem.close().await.expect("close");
}
