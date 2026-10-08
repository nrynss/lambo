//! Level B's single construction site on the serve path: the one resolve a
//! serve process performs ([`resolve_serve_backends`]) and the one
//! [`MemoryBuilder`](crate::MemoryBuilder) it configures from the result
//! ([`serve_builder`]). [`build_memory`] is the library entry point over the
//! same builder.

use std::path::Path;
use std::sync::Arc;

use super::{EarlyShutdown, ServeOptions};
use crate::ledger::Ledger;
use crate::mcp::endpoint::SessionEndpoint;
use crate::memory::Memory;
use crate::resolve::{resolve_from_config_path, ResolvedBackends};
use crate::types::LamboError;

/// The one resolve a serve process performs (Level B).
///
/// Thin by design — it exists so the single construction site is named and
/// greppable, not to add behaviour. Config precedence (`--config`, then
/// `LAMBO_CONFIG`, then `./lambo.toml`, then defaults; env overrides file) and
/// every fail-closed check live inside `resolve_from_config_path`.
pub fn resolve_serve_backends(config: Option<&Path>) -> Result<ResolvedBackends, LamboError> {
    resolve_from_config_path(config).map_err(|e| LamboError::Config(e.to_string()))
}

/// Build the single [`Memory`] this process owns from an **already-resolved**
/// [`ResolvedBackends`].
///
/// **Level B, single construction site.** This function deliberately does *not*
/// resolve: the caller resolves once and hands the result in, so there is
/// exactly one store and one embedder per process and no second config pass.
/// Fail-closed behaviour — uncompiled `kind`, unknown TOML key, store×embedder
/// dim mismatch — lives in that one resolve; see [`resolve_serve_backends`].
///
/// # `serve` does not call this any more (J2-R1-7)
///
/// It is a **library entry point**, kept because it is `pub` and re-exported at
/// `crate::mcp`, and because "build the one `Memory` a serve-shaped process
/// owns, with the `[daemon]` cadence applied" is a useful thing for an embedder
/// to be able to ask for in one call. J2 replaced the serve path's use of it
/// with `serve_builder` plus `resolve_role`, because the startup election has
/// to retry the *attach* against the same configuration and therefore needs the
/// builder rather than the built `Memory`. `rg build_memory` finds no call site
/// in this tree.
///
/// The consequence for a reader: comments describing serve startup name
/// `resolve_role`, not this function. The round-1 review found nine sites that
/// still named this one; they were rewritten in the same commit as this
/// paragraph.
///
/// [`ResolvedBackends`]: crate::resolve::ResolvedBackends
pub async fn build_memory(
    opts: &ServeOptions,
    backends: ResolvedBackends,
    endpoint: Option<&SessionEndpoint>,
) -> Result<Memory, LamboError> {
    // Cadence overrides from `[daemon]` reach the writer here. Without this the
    // daemon always runs at Config::default() and `gc_interval` in lambo.toml
    // would parse, validate, and then do nothing at all.
    // J6: no pre-arm. This is the *library* entry point (`serve` has not called
    // it since J2-R1-7), and installing process-wide signal handlers is a
    // decision that belongs to a process, not to a builder — the same reason
    // `close_bounded_until` takes its re-armed signal as an argument. An
    // unarmed handle registers nothing and never fires.
    serve_builder(opts, backends, endpoint, None, EarlyShutdown::unarmed())
        .build()
        .await
        .map_err(explain_startup_failure)
}

/// The one [`MemoryBuilder`](crate::MemoryBuilder) a serve process configures.
///
/// Split out of [`build_memory`] so J2's startup election can retry the attach
/// against the **same** configuration: `MemoryBuilder` is `Clone` and every
/// backend inside it is an `Arc`, so a retry is a clone rather than a second
/// resolve. Level B's single construction site is unchanged — `main` still
/// resolves once and this is still the only place `Memory::builder()` is called
/// on the serve path. Since that split, this — not `build_memory` — is what the
/// serve path uses; `build_memory` became a library-only entry point that
/// delegates here (J2-R1-7).
pub(super) fn serve_builder(
    opts: &ServeOptions,
    backends: ResolvedBackends,
    endpoint: Option<&SessionEndpoint>,
    ledger: Option<Arc<Ledger>>,
    early: EarlyShutdown,
) -> crate::memory::MemoryBuilder {
    let config = backends.config.clone();
    let mut builder = Memory::builder()
        .session(opts.session.clone())
        .agent(opts.agent.clone())
        .config(config);
    // J2: published by the acquire that takes the lease, so a live row always
    // names the current holder's address. The socket itself is bound by the
    // caller AFTER this returns — see `authorize_bind`. `None` (a store no
    // second process can see) publishes nothing, which is the honest row for a
    // holder nothing can reach.
    if let Some(endpoint) = endpoint {
        builder = builder.endpoint(endpoint.published());
    }
    // J4: the ledger this process opens pre-lease (so its own conflict and
    // write-intent completion lines ride it) is handed straight into the
    // Memory it builds.
    builder = builder.ledger(ledger);
    // J6: armed by the acquire itself, from inside `build_attach`. Handed in
    // here because this is the one place the serve path configures the builder,
    // and the acquire is the only point at which arming is both safe (the
    // election is over) and necessary (a lease and a tail now exist).
    builder = builder.early_shutdown(early);
    builder.backends(backends)
}

/// Turn a raw driver error at attach time into an actionable message.
///
/// Pointing `serve` at a fresh SQLite file or an unmigrated Cockroach database
/// failed with nothing but `no such table: sessions` (R1/T82-10). Schema
/// bootstrap belongs to `lambo provision` (T8.3) and `serve` deliberately does
/// not auto-init — but the *message* is T8.2's, and "run provision" is the one
/// thing the operator needs to be told.
pub(super) fn explain_startup_failure(err: LamboError) -> LamboError {
    let text = err.to_string();
    let lower = text.to_lowercase();
    // The shapes the two SQL backends use for "the schema isn't there":
    // SQLite says `no such table`, Postgres/Cockroach say `relation "x" does
    // not exist` (SQLSTATE 42P01) or `undefined_table`.
    let unprovisioned = lower.contains("no such table")
        || lower.contains("does not exist")
        || lower.contains("undefined_table")
        || lower.contains("42p01");
    if unprovisioned {
        LamboError::Config(format!(
            "session store is not provisioned — run 'lambo provision' \
             (or scripts/provision.sh) against this store first, then retry. \
             Underlying error: {text}"
        ))
    } else {
        err
    }
}
