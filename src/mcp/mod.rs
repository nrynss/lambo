//! MCP tool surface for `lambo serve` (P8, spec §6.2).
//!
//! `lambo serve --session S --transport stdio|http` exposes seven tools —
//! `lambo_recall`, `lambo_derive`, `lambo_record_action`, `lambo_reserve`,
//! `lambo_inspect`, `lambo_saints`, `lambo_stats` — over the
//! [rmcp](https://docs.rs/rmcp) server SDK.
//!
//! This is *Lambo's* MCP server, which agents use to **write** memory. It is
//! not CockroachDB's managed MCP server, which is separate, read-only, and used
//! to inspect the store underneath (spec §12.1).

pub mod serve;
pub mod server;

pub use serve::{
    build_memory, resolve_auth_token, resolve_serve_backends, serve, SecretToken, ServeOptions,
    Transport, AUTH_TOKEN_ENV, DEFAULT_MAX_SESSIONS, DEFAULT_RATE_LIMIT_RPS,
};
pub use server::LamboServer;

/// Initialise logging for a serve process.
///
/// **Diagnostics go to stderr, always.** Under `--transport stdio`, stdout is
/// the JSON-RPC channel: a single stray log line on it corrupts the framing and
/// the client drops the connection. Honouring `RUST_LOG` keeps the default
/// quiet enough for a client to launch this as a subprocess.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("lambo=info,rmcp=warn"));
    // A second `serve` in one process (tests) must not panic on re-init.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
