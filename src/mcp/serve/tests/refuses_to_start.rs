use super::*;
use crate::embed::FixtureEmbedder;
use crate::store::{MemoryStore, StoreConfig};
use crate::types::EmbeddingContract;

fn backends() -> ResolvedBackends {
    ResolvedBackends {
        store: Box::new(MemoryStore::new()),
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: StoreConfig {
            kind: Default::default(),
            dsn: None,
            path: None,
            vector_dim: None,
        },
        embedder_cfg: Default::default(),
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config: crate::Config::default(),
    }
}

#[tokio::test]
async fn serve_refuses_an_unauthenticated_non_loopback_bind() {
    let opts = ServeOptions {
        transport: Transport::Http,
        // The shape that actually ships by accident: bind-all in a
        // container, no token, reachable from the network.
        bind: "0.0.0.0".parse().expect("addr"),
        port: 0,
        ..ServeOptions::new("t87-refuse", "agent-a")
    };
    let err = serve(opts, backends())
        .await
        .expect_err("serve must refuse to start, not come up unauthenticated");
    let msg = err.to_string();
    assert!(msg.contains("refusing to start"), "{msg}");
    assert!(msg.contains(AUTH_TOKEN_ENV), "{msg}");
}

// The two *positive* legs — an HTTP bind that a token satisfies, and a
// stdio serve that ignores `--bind` entirely — are pinned by the unit
// tests on `authorize_bind` rather than through `serve`, deliberately.
// Driving either one end-to-end means actually starting a server in the
// test binary: the HTTP leg would bind 0.0.0.0 (firewall prompts, and a
// listening socket on every interface of a CI box), and the stdio leg
// would park a *blocking* read on the test harness's stdin — which
// `Runtime::drop` then waits for, hanging the suite on any machine where
// stdin is a TTY instead of EOF. Neither risk buys coverage the unit
// tests do not already give.

/// #32 PR 6 review L5: a library caller's `idle_detach` of zero is refused
/// before any lease, as `[serve] idle_detach_secs = 0` is.
#[tokio::test]
async fn serve_refuses_a_zero_idle_detach() {
    let mut opts = ServeOptions {
        transport: Transport::Http,
        port: 0,
        ..ServeOptions::new("pr6-l5-refuse", "agent-a")
    };
    opts.bounds.idle_detach = Duration::ZERO;
    let err = serve(opts, backends())
        .await
        .expect_err("a zero idle_detach is refused");
    assert!(err.to_string().contains("idle_detach"), "{err}");
}
