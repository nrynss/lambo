//! `lambo serve` — process lifecycle for the MCP server.
//!
//! Each session has exactly one owning process; a process may own many
//! sessions (spec §2.2 as amended by #32's design, decision 18). This module
//! builds every [`Memory`] it serves from **one** [`ResolvedBackends`], serves
//! them over stdio (one session) or streamable HTTP (one or more pinned
//! sessions, routed by path), and guarantees [`Memory::close`] runs on the
//! way out so every final flush happens.
//!
//! # The lifecycle, in one place
//!
//! [`serve`] is the composition root and the order lives in its body; every
//! step it calls is in a child module. In order:
//!
//! 1. **Pre-lease group** (creates nothing a retry could trip on, runs under
//!    the default signal disposition): `authorize_bind`, [`authorize_ledger`],
//!    the endpoint derivation (`hub::derive_endpoint`), `Ledger::open` and its
//!    `startup` line, the keep-warm interval, the unarmed `EarlyShutdown`,
//!    `serve_builder`.
//! 2. **Election** (`roles::resolve_role`): the only lease acquire on the serve
//!    path. A loser that can forward becomes a proxy (`HubProxy::run`, then its
//!    ledger drains and it returns); the pre-arm is armed inside the acquire.
//! 3. **Holder startup**, below the arming (`shutdown::holder_shutdown`):
//!    `LamboServer` (`session::session_server`), the process-wide tasks
//!    (`process::ProcessTasks::spawn`: the ledger heartbeat, the #13
//!    keep-warm, the J4 refusal poller), the rest of the session
//!    (`session::AttachedSession::attach`: the session endpoint through
//!    `hub::bind_hub`, the event pump), the `session attached` line.
//! 4. **Transport** (`transport`): stdio or HTTP, each behind the T8.7 guards
//!    (`http_guards`) where they apply, until a signal, a lease loss or the
//!    client ends it.
//! 5. **Shutdown**, the seven named stages in `shutdown`: transport drain,
//!    keep-warm abort, the bounded session close, event pump, background
//!    tasks, endpoint release, ledger close. Each logs when it starts and
//!    finishes (`stages`, #40). Stages 3, 4 and 6 run over the attached set
//!    of sessions (`registry::SessionRegistry`).
//!
//! With more than one pinned session (HTTP only, #32 PR 4) steps 2 and 3
//! differ: there is no election, each pinned session is acquired from the
//! one template builder (`registry::SessionRegistry::acquire`), a session
//! held elsewhere is retried in the background, and a lost lease detaches
//! that session instead of ending the process (`registry::LeaseLossPolicy`).
//!
//! # Process part and session part (#32)
//!
//! What a serve process has once (the embedder and store in the backends,
//! the listener and transports, the HTTP guards, the signals and
//! `EarlyShutdown`, the watchdog, the ledger file, and
//! `process::ProcessTasks`) is kept apart from what each session it holds
//! has (`session::AttachedSession`: the `Memory` with its lease and
//! heartbeat, the `LamboServer`, the endpoint `Hub`, `session::SessionTasks`).
//! A single-session serve builds one of the latter; a multi-session serve
//! (#32 PR 4) builds one per pinned session and holds them in a
//! `registry::SessionRegistry`.
//!
//! # Modules
//!
//! | module | holds |
//! |---|---|
//! | `builder` | the one resolve ([`resolve_serve_backends`]), `serve_builder`, [`build_memory`] |
//! | `roles` | the startup election, `Role`, the loser-side refusal record |
//! | `pinned` | which sessions a serve pins ([`pin_sessions`]) and `serve`'s own check of them |
//! | `registry` | the attached sessions (`SessionRegistry`): slots, the pinned attach, the detach, the routing lookup, the lease-loss policy |
//! | `process` | the process-wide background tasks (`ProcessTasks`) |
//! | `session` | the per-session part (`AttachedSession`, `SessionTasks`) |
//! | `hub` | every Unix-socket touch: endpoint derivation, bind, accept loop, release, the proxy probe (the #39 seam) |
//! | `heartbeat` | ledger configuration, the heartbeat, the startup line, the holder refusal poller, the event pump |
//! | `http_guards` | the bearer token, the bind refusal, the rate limit, the session cap, the body ceiling |
//! | `transport` | stdio, streamable HTTP and its `/mcp` + `/mcp/s/{session}` router, the bounded wind-down both share |
//! | `signals` | the eager signal registration and J6's pre-arm (`EarlyShutdown`) |
//! | `shutdown` | the grace budgets, the shutdown future, the close, the named stages (the #40 seam) |
//! | `stages` | the stage record and its `started` / `finished` log lines (#40) |
//! | `watchdog` | the OS-thread bound on the whole shutdown, independent of the runtime (#40) |

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::types::LamboError;
use crate::writeq::EmbedderCalibration;

mod builder;
mod heartbeat;
mod http_guards;
mod hub;
mod pinned;
mod process;
mod registry;
mod roles;
mod session;
mod shutdown;
mod signals;
mod stages;
mod transport;
mod watchdog;

pub use builder::{build_memory, resolve_serve_backends};
pub use heartbeat::authorize_ledger;
pub use pinned::{pin_sessions, PinnedSessions};

use builder::{explain_startup_failure, serve_builder};
use heartbeat::serve_startup_line;
use http_guards::authorize_bind;
pub use http_guards::{
    resolve_auth_token, SecretToken, AUTH_TOKEN_ENV, DEFAULT_MAX_SESSIONS, DEFAULT_RATE_LIMIT_RPS,
};
use pinned::check_pinned;
use process::ProcessTasks;
use registry::{Acquired, LeaseLossPolicy, SessionAttacher, SessionRegistry};
use roles::{resolve_role, Role};
use session::{session_server, AttachedSession};
use shutdown::{
    close_bounded, close_ledger, close_sessions, holder_shutdown, join_all, registry_shutdown,
    stop_transport,
};
use signals::shutdown_signal;
use stages::Stage;
use transport::{serve_http, serve_stdio};

pub(crate) use signals::EarlyShutdown;
pub(crate) use stages::ShutdownProgress;
// The crate paths the `memory` and `writeq` tests name; nothing outside a
// test build reads them through `serve`, so each is gated like its readers.
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
pub(crate) use shutdown::close_bounded_until;
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
pub(crate) use shutdown::run_and_close;
#[cfg(test)]
pub(crate) use shutdown::CLOSE_FLUSH_GRACE;
// The unit tests reach the server type through this module's glob; nothing
// in `serve` itself names it since the session part moved to `session`.
// Gated like its readers (the heartbeat and hub-release tests).
#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
use crate::mcp::server::LamboServer;

/// Which transport `lambo serve` should listen on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout — what an MCP client
    /// launching `lambo serve` as a subprocess speaks.
    Stdio,
    /// Streamable HTTP (`POST /mcp`), for the T8.5 demo app and remote clients.
    Http,
}

impl std::str::FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            other => Err(format!(
                "unknown transport '{other}' (expected 'stdio' or 'http')"
            )),
        }
    }
}

/// Everything `serve` needs that is not in `lambo.toml`.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// The default session: the one a stdio serve owns and `/mcp` serves.
    /// Must be one of [`ServeOptions::sessions`].
    pub session: String,
    /// Every session this process pins, in order (#32 PR 4): attached at
    /// startup and held until it exits. [`ServeOptions::new`] pins
    /// `session` alone; [`pin_sessions`] builds both fields from the
    /// command line and `[serve]`. More than one needs `Transport::Http`, and
    /// each is then served at `/mcp/s/{session}`.
    pub sessions: Vec<String>,
    /// Agent identity this process writes as. See the attribution note on
    /// [`LamboServer`](crate::mcp::server::LamboServer) — `Memory` binds one
    /// agent per session handle.
    pub agent: String,
    pub transport: Transport,
    pub port: u16,
    /// Bind address for `--transport http`. Defaults to loopback. Binding
    /// anywhere else requires [`ServeOptions::auth_token`] — see
    /// `authorize_bind`.
    pub bind: IpAddr,
    /// Bearer token required on every HTTP request (T8.7). `None` is allowed
    /// only on loopback; [`AUTH_TOKEN_ENV`] overrides whatever the flag said.
    pub auth_token: Option<SecretToken>,
    /// Ceiling on concurrently live MCP sessions.
    pub max_sessions: usize,
    /// Sustained requests/second on the HTTP transport; `0` disables the limit.
    pub rate_limit_rps: u32,
    /// I1 — append one JSONL line per MCP tool call to this path. `None` (the
    /// default) is off: no writer thread, no per-call facts, and `lambo_stats`
    /// reports the payload it reported before the ledger existed.
    pub ledger: Option<PathBuf>,
    /// I2 — append a `stats` heartbeat line on this interval. Requires
    /// [`ServeOptions::ledger`]; `None` is off.
    pub ledger_heartbeat: Option<Duration>,
}

impl ServeOptions {
    pub fn new(session: impl Into<String>, agent: impl Into<String>) -> Self {
        let session = session.into();
        Self {
            sessions: vec![session.clone()],
            session,
            agent: agent.into(),
            transport: Transport::Stdio,
            port: 7700,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            auth_token: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            rate_limit_rps: DEFAULT_RATE_LIMIT_RPS,
            ledger: None,
            ledger_heartbeat: None,
        }
    }
}

/// Run the MCP server to completion, then close the session.
///
/// [`Memory::close`] runs on **every** exit path — clean client disconnect,
/// SIGINT/SIGTERM, or a transport error — because the tail is only durable once
/// it has run. Its error is surfaced: an `Err` from `close()` means the tail is
/// *not* durable and was kept (T8.1 semantics).
///
/// **The single-writer lease (T8.6) rides the same exit paths.** `resolve_role`
/// acquired the lease as this process attached — failing closed here if another
/// writer holds it *and* cannot be proxied to, and returning `Role::Proxy`
/// rather than reaching this function if it can (J2). A successful `close()`
/// **releases**
/// it so the next writer takes over at once. On the one exit that abandons
/// `close()` — the `close_bounded` timeout or a second signal — the lease is
/// *not* released and instead lapses at [`lease::LEASE_TTL`](crate::store::lease::LEASE_TTL), exactly as it would
/// on a crash. That is why the TTL is sized to outlast `SHUTDOWN_BUDGET`: a
/// graceful-but-slow close still holds a valid lease at the moment it releases.
///
/// Both transports route their shutdown through `run_until_shutdown`, so the
/// signal path is the same on each: cancel, wait up to `SHUTDOWN_GRACE`, then
/// drop the transport and close regardless. A client that will not let go
/// cannot hold the tail hostage.
///
/// The shutdown signal is armed **before** the transport handoff (R2-a): a
/// continuously-live registration threads through the pre-handshake window (the
/// stdio handshake, the HTTP `bind`) and the transport itself, so a signal in
/// *any* of those still reaches [`Memory::close`] instead of hitting the default
/// disposition and killing the process with the tail un-flushed.
///
/// **Two registrations, not one** (J6). It said "a single" until the pre-arm,
/// and the count is the whole point: the arming below happens the first
/// statement after `resolve_role` returns, but the lease is taken *inside* it,
/// and the span between the two used to run under the default disposition — a
/// SIGTERM there killed the process with `close()` un-run (CI run 32710994512,
/// `unix_wait_status(15)`). `EarlyShutdown` is armed at the acquire itself and
/// only records; `wind_down` reads the record alongside the fresh signal, so a
/// signal in that span cancels the transport on its first poll instead. Neither
/// registration covers the startup election above the acquire, which is what
/// keeps a serve waiting out `ELECTION_BUDGET` killable (J2-R1-7).
pub async fn serve(opts: ServeOptions, backends: ResolvedBackends) -> Result<(), LamboError> {
    // T8.7, and it runs FIRST — before `resolve_role`, which attaches to the
    // store and takes the single-writer lease. A misconfigured bind must cost
    // nothing and leave nothing behind: refusing here means no lease is taken,
    // so the operator's retry after setting a token is not blocked by the
    // lease their own refused start would otherwise be holding.
    authorize_bind(opts.transport, opts.bind, opts.auth_token.as_ref())?;
    // Same argument as the bind check: refuse before any lease is taken.
    authorize_ledger(&opts)?;
    // #32 PR 4: the pinned sessions, refused before any lease too.
    check_pinned(&opts.session, &opts.sessions, opts.transport)?;
    if LeaseLossPolicy::for_pinned(opts.sessions.len()) == LeaseLossPolicy::DetachSession {
        return serve_pinned(opts, backends).await;
    }
    // J2, and it belongs in this pre-lease group for the same reason the two
    // above do: it creates nothing and binds nothing (its one filesystem access
    // is a read-only `canonicalize` of the store path, which is what makes the
    // derived address a store IDENTITY rather than a store spelling — J2-R1-2),
    // so deriving it costs nothing and leaves no lease behind. See
    // `authorize_bind`'s "What J2 changed" section, which restates that claim
    // rather than letting unconditional binding quietly falsify it.
    //
    // It cannot refuse the start (J2-R1-5): an unusable path degrades to `None`,
    // logged at ERROR inside `for_store`, and this process serves its own client
    // exactly as it did before J2. A losing serve on such a machine then refuses
    // as it did before J2 too, because a proxy needs the holder to have bound —
    // one client working instead of none.
    let endpoint = hub::derive_endpoint(&opts.session, &backends.store_cfg);

    // J4 — the ledger opens HERE, before the acquire attempt inside
    // `resolve_role`. That is the whole of J4's first half: a serve that LOSES
    // the lease exits before it can reach the holder path below, where the
    // ledger used to open, so the acquire itself could not be where a losing
    // serve's story starts. Opening it pre-lease and writing the startup line
    // means a serve about to lose the lease has already left an artifact.
    // `Ledger::open` never fails or blocks the caller, so this is free to move
    // into the pre-lease group; `authorize_ledger` above still refuses the
    // misconfigured `--ledger-heartbeat`-without-`--ledger` pairing first.
    // #32 decision 15: scoped to the session, so every line this serve
    // appends (call, completion, heartbeat; startup and lease lines already
    // name theirs) says which session it is about.
    let ledger = opts
        .ledger
        .as_ref()
        .map(|path| Ledger::open(path.clone()).for_session(&opts.session));
    if let Some(ledger) = &ledger {
        ledger.append(&serve_startup_line(&opts));
    }
    // Issue #13 — read before `serve_builder` consumes the backends. Pure (a
    // config lookup and a type downcast), no I/O, so it belongs in this
    // create-nothing pre-lease group; the task it configures is spawned on the
    // holder path only, below the arming.
    let keep_warm = backends.keep_warm_interval();
    // #32 PR 3 — the write queue's calibration, once per process (design
    // decision 14): the probe measures the embedder, which every session this
    // process attaches shares. Creating it probes nothing and holds no
    // embedder (the first pipeline built spawns the probe, and the key is
    // weak), so it belongs in this pre-lease group and a proxy that never
    // builds a pipeline never probes and keeps no model.
    let calibration = EmbedderCalibration::new();
    // J6 — the pre-arm. Constructing it installs NOTHING; it is armed from
    // inside `build_attach`, in the `LeaseOutcome::Acquired` arm, so the
    // election below stays killable and a serve that loses never arms at all.
    // See `EarlyShutdown` for the window it closes and why the arming point
    // could not simply move above `resolve_role` (J2-R1-7).
    let early = EarlyShutdown::unarmed();
    let builder = serve_builder(
        &opts,
        backends,
        endpoint.as_ref(),
        ledger.clone(),
        early.clone(),
        Some(calibration.clone()),
    );
    // #32 PR 4 (review L1): the process's one embedder, for the keep-warm,
    // taken from the backends rather than from the session. Dropped on
    // every branch that does not hold the session, so a proxy still keeps
    // no model (`resolve_role`'s argument).
    let embedder = builder.shared_embedder();
    // Moved, not lent: see `resolve_role` — a proxy must not keep the model.
    let role = resolve_role(&opts, builder, endpoint.as_ref(), &ledger).await;
    let mem: Arc<Memory> = match role {
        // The startup election refused (or otherwise failed): this process's
        // pre-lease ledger was opened and the loser recorded its `startup` and
        // `lease:refused` lines — DRAIN it before exiting, or the very artifact
        // J4 exists to leave would sit unflushed in the writer thread's channel
        // and die with the process. This is the exit the pre-lease line was
        // written for, so it is the one that must not drop it.
        Err(e) => {
            if let Some(ledger) = &ledger {
                ledger.shutdown();
            }
            return Err(e);
        }
        Ok(Role::Holder(mem)) => Arc::from(mem),
        Ok(Role::Proxy(proxy)) => {
            drop(embedder);
            // **The proxy branch is deliberately NOT armed for durability, and
            // this is the design decision the J0 review asked for by name.**
            //
            // The hazard it warned about is real: a refused serve never reaches
            // the holder path below, so naive proxy code would run above the
            // arming point — I-R2-1's pre-handshake hole through a new door. The
            // answer is not to move the arming but to notice that *the hole is
            // not there*. What that arming protects is `Memory::close`: a lease
            // taken, an in-RAM write-behind tail, a graph. A proxy has none of
            // the three. It holds no lease (`resolve_role` returned this branch
            // precisely because it lost), no tail (every write happens inside
            // the holder, under the holder's fencing token) and no graph. There
            // is nothing a signal handler could save, so arming for durability
            // would be theatre — a handler that logs.
            //
            // A registration is still installed, for **liveness**: it is how the
            // pump's `select!` learns to stop, so SIGTERM ends the proxy with a
            // log line and a closed socket instead of a bare kill. It is polled
            // first in that `select!`, so this does not make the process
            // SIGTERM-immune the way arming above the lease-taking attach
            // would.
            //
            // **J6 asked whether this branch has the holder's window too, and
            // it does not.** The holder's window is real because the span from
            // the acquire to the arming contains the whole tail of the `Memory`
            // build — including the "Memory session attached" line the
            // pre-handshake test signals on — so a descheduled process can sit
            // in it for milliseconds. This branch's equivalent span is
            // `resolve_role` returning `Role::Proxy`, the match above, and the
            // evaluation of `shutdown_signal()` as the argument on the next
            // line: **no `await` at all**, and no log line a test could
            // synchronise on (the proxy's own sync point, "proxying to the
            // session holder", is logged from inside `run`, already under the
            // registration). Even granting the span, the durability argument
            // above still applies unchanged — no lease, no tail, no graph — so
            // there is nothing for a pre-arm to save. The one thing a kill here
            // does cost is the pre-lease ledger lines, and those are already
            // forfeit to any signal during the election above, which must stay
            // killable. So: no pre-arm on this branch, deliberately.
            //
            // The wedge invariant is what makes that safe to state so flatly.
            // `EarlyShutdown::arm` is called from the `LeaseOutcome::Acquired`
            // arm of `build_attach` and nowhere else, so "a proxy never arms"
            // is not a second rule to keep true — it is "a proxy never takes
            // the lease", read through the signal disposition.
            //
            // Nothing else on this branch is skipped by accident (J4): the
            // ledger this process opened **pre-lease** IS passed into the proxy
            // (`HubProxy::new`), which now books its own `proxying` /
            // `proxying_stopped` lines on it — a proxy is alive and can write,
            // which is the whole of §J2's J4 handoff. What it still does not do
            // is spawn a heartbeat, bind a socket or build a `LamboServer`. It
            // is a pipe.
            let outcome = proxy.run(shutdown_signal()).await;
            tracing::info!("lambo serve: proxy closed (no lease was ever taken by this process)");
            // J4: the ledger this process opened pre-lease was used by the
            // proxy's own `proxying` / `proxying_stopped` lines; drain it
            // before exit. (This branch returns before the holder-path shutdown
            // below, so exactly one of the two runs.)
            if let Some(ledger) = &ledger {
                ledger.shutdown();
            }
            return outcome;
        }
    };

    // The signal registration for the whole life of the transport, armed HERE —
    // the first statement after the lease-taking attach returns (`resolve_role`,
    // which builds through the same `serve_builder` `MemoryBuilder`), and
    // before ANY of the startup work below it: `LamboServer::new` and its
    // `#[tool_router]` JSON-schema build, the heartbeat spawn, the issue-13
    // embedder keep-warm spawn, the J4 refusal-recorder spawn, the J2
    // session-endpoint bind and its accept loop, the event pump, and the
    // serve-level attach log. Registration is eager (see
    // `shutdown_signal`), so every one of those runs guarded (R2-a): a SIGTERM
    // arriving during them is taken by this future, `Memory::close` runs, the
    // tail reaches the store, and the single-writer lease is released.
    //
    // **`Ledger::open` is NOT in that list any more, and its absence is the
    // load-bearing correction** (JE2E-6). J4 moved it, and the startup line it
    // writes, into the pre-lease group above `resolve_role`, so both run
    // *unguarded*. Harmless, and for a reason rather than by luck: `Ledger::open`
    // spawns a thread and returns — it performs no I/O of its own, which is the
    // guarantee `opening_a_ledger_does_not_block_even_when_the_paths_open_blocks`
    // pins — so there is no window in it for a signal to be deferred across.
    // What the enumeration is FOR is the R2-a claim, and a stale enumeration
    // makes that claim wrong in the direction that matters: it credits the
    // arming with covering work it no longer covers.
    //
    // The precise property, stated where the previous comment overclaimed
    // (I-R2-1): *this* guard begins the instant `resolve_role` returns. It does
    // NOT cover `resolve_role` itself, so the memory-level "Memory session
    // attached (daemon + flush + canonization running)" line — emitted from
    // inside the `Memory` build, after the lease is taken — is followed by a
    // residual window until this arming. The serve-level "lambo serve: session
    // attached" line below is covered by this one. The earlier wording claimed
    // the stronger property for both lines; it was false for the memory-level
    // one, and I moving `LamboServer::new` up from inside `serve_stdio` widened
    // that residual window from ~6 µs to ~1.1 ms, which is the durability
    // regression I-R2-1 records.
    //
    // **That residual window is no longer unguarded, and J6 is what closed it.**
    // It said "still unguarded ... exactly as it was pre-I" until CI run
    // 32710994512 collected on it: `a_pre_handshake_sigterm_still_flushes_the_
    // session_row` failed with `unix_wait_status(15)` — the process KILLED by
    // the signal, not exited on it, so `close()` never ran and the tail died.
    // Not a flake; timing variance on a loaded runner against a real window that
    // three rounds had priced and accepted. [`EarlyShutdown`] now arms at the
    // acquire, inside `build_attach`, and only RECORDS; `wind_down` reads the
    // record beside the fresh signal below, so a SIGTERM in the residual window
    // makes the transport's very first poll of the shutdown future ready.
    //
    // Arming *before* the attach would shrink the residual window to zero too,
    // and pre-J2 the argument against it was a trade: a durability hazard for an
    // availability one, since the signal would be deferred rather than honoured
    // until a hung build finished. **J2 makes that argument much stronger, and
    // this is the sentence the round-1 review found missing (J2-R1-7).** The
    // thing that would be made SIGTERM-immune is no longer `build_memory` —
    // which `serve` does not call at all any more — but `resolve_role`, and
    // `resolve_role` is a loop that can *legitimately* run for the whole of
    // `ELECTION_BUDGET` = **20 seconds**: that is its designed behaviour when
    // the holder it lost to is not proxyable yet, not a hang.
    //
    // (This said 50 seconds — `LEASE_TTL + ELECTION_SLACK` — until JE2E-7. That
    // WAS the budget; J2-L2 cut it to a client-tolerance number, and
    // `ELECTION_BUDGET`'s own docstring has said the 50s formula "used to be"
    // the rule ever since, in this same file. The argument survives the true
    // number and is re-made at it below, which is the only thing that makes
    // restating it worthwhile.)
    //
    // Twenty seconds of unkillable wait is still the wrong trade, and by a wide
    // margin: it would be spent to close a ~1.1 ms durability window in a
    // process that holds no lease and no tail while it waits — four orders of
    // magnitude of deliberate deafness bought with none of the thing being
    // protected. And the 20s is not a worst case that rarely fires; the
    // dead-holder election measured 10.2s live at the two-client probe, so it
    // is an ordinary start. **That ruling stands, and J6 obeys it rather than
    // overturning it**: the pre-arm sits BELOW the acquire, so the election is
    // over before anything is registered — a serve still waiting for a lease
    // still dies to a SIGTERM, and a serve that loses and proxies never arms at
    // all. This paragraph used to end "the residual window is real and worth
    // closing, but only by racing the attach against the shutdown future, which
    // is a design change and is deferred; see I-R2-1's recommendation". That is
    // what J6 did, at the one `await` where it matters: the startup load inside
    // `build_attach` is raced against the record, so the pre-arm covers no
    // unbounded wait and the immunity is not re-created one `await` down.
    //
    // A fresh registration in `close_bounded` re-arms it for the close phase.
    //
    // JE2E-4: and the *other* thing this process winds down for. `wind_down`
    // races the signal against the lease fence, so losing the lease ends the
    // transport by the same route SIGTERM does. `shutdown_signal()` is still
    // CALLED here, so its eager registration is unmoved — only the polling is
    // wrapped; the contract is documented at `shutdown_signal` itself, which is
    // the function it is a property of (JE2E-R2-1).
    //
    // The ledger goes in too (JE2E-R2-4), so the fence arm can book the
    // `lease:lost` line before it cancels the transport.
    //
    // **This line is the whole of the ruling's behaviour**, and severing it —
    // passing a bare `shutdown_signal()` here while still constructing
    // `wind_down` — used to pass the entire suite (JE2E-R2-2). It is now pinned
    // by `serve_feeds_the_fence_into_the_transports_shutdown`, which drives
    // `run_transport_until_shutdown` the way this call site does.
    // Cloned, not moved: the same signal record is read twice on this path —
    // by `wind_down` for the shutdown itself, and by `close_bounded` for the
    // "was that a SECOND Ctrl-C?" escape hatch. Both readers must see one
    // count, which is exactly why `EarlyShutdown` is `Clone` over shared state.
    // #40: the shutdown's stage record, shared by the shutdown future (which
    // starts stage 1), `run_and_close_sessions` (stages 1 to 4) and the tail below.
    // Its first stage starts the watchdog, an OS thread that aborts the
    // process if the shutdown outlives every one of its own timers; the
    // guard stands it down on every way out of this function.
    let progress = ShutdownProgress::with_production_watchdog();
    let _disarm = progress.disarm_on_drop();
    let shutdown = holder_shutdown(mem.clone(), ledger.clone(), early.clone(), progress.clone());
    tokio::pin!(shutdown);

    // #32 PR 4: a registry of one, under the lease-loss policy a single
    // session has always had (the fence above ends the process).
    let registry = SessionRegistry::new(
        vec![opts.session.clone()],
        Some(opts.session.clone()),
        LeaseLossPolicy::ExitProcess,
        None,
        early.clone(),
    );
    // The holder startup, below the arming, in the order it has always run:
    // the session's server, the process-wide tasks (which read it), then the
    // rest of the session (its endpoint and event pump).
    let server = session_server(&mem, &ledger);
    // Stopped after the close (and the keep-warm before it); see
    // `process::ProcessTasks` and the stage table in `shutdown`.
    let embedder = embedder.unwrap_or_else(|| Arc::clone(mem.embedder()));
    let tasks = ProcessTasks::spawn(
        &registry,
        &ledger,
        opts.ledger_heartbeat,
        keep_warm,
        &embedder,
        mem.agent(),
        &calibration,
    );
    drop(embedder);
    // `mem` stays held here as well, so the last handle still drops when
    // `serve` returns, after the watchdog is disarmed (the stage table's
    // "not watched" note), not when the set is taken apart at stage 6.
    let session = AttachedSession::attach(Arc::clone(&mem), server, endpoint, opts.max_sessions);
    registry.insert_live(Arc::new(session));
    registry.mark_started();

    tracing::info!(
        session = %opts.session,
        agent = %opts.agent,
        transport = ?opts.transport,
        "lambo serve: session attached"
    );

    let stdio_server = registry.attached()[0].server.clone();
    let transport = async {
        match opts.transport {
            Transport::Stdio => serve_stdio(stdio_server, shutdown.as_mut()).await,
            Transport::Http => serve_http(registry.clone(), &opts, shutdown.as_mut()).await,
        }
    };
    close_holder(&registry, transport, tasks, &early, &progress, ledger).await
}

/// `serve` for more than one pinned session (#32 PR 4): HTTP only, under
/// [`LeaseLossPolicy::DetachSession`].
///
/// The same pre-lease group, arming, process tasks, transport and shutdown
/// stages as a one-session serve; what differs is the attach. There is no
/// election: each pinned session is acquired, in order, from the one
/// `serve_builder` template (so every session shares the embedder, the
/// store and the write-queue calibration), and a session another writer
/// holds is not waited for but served as 503 and retried in the background
/// (design §3.2). Any other attach failure refuses the start, after closing
/// (and so releasing) the sessions already acquired.
///
/// The arming argument is `serve`'s, unchanged: J6's pre-arm arms at the
/// first acquire and every later load races it; the shutdown future is
/// registered once the pinned acquires are done, before any session part is
/// built.
async fn serve_pinned(opts: ServeOptions, backends: ResolvedBackends) -> Result<(), LamboError> {
    serve_pinned_with(opts, backends, PinnedSeams::default()).await
}

/// What a test hands [`serve_pinned_with`] so it can drive the real
/// multi-session serve in-process. `serve` passes the default: a fresh
/// unarmed pre-arm and nobody to tell.
#[derive(Default)]
struct PinnedSeams {
    /// The J6 pre-arm the serve uses. A test keeps a clone and records a
    /// signal on it (`EarlyShutdown::simulate_signal`) to shut the serve
    /// down without sending the test process a real one.
    early: Option<EarlyShutdown>,
    /// Sent the registry once the startup sessions are in and the retry
    /// loop is running.
    registry: Option<tokio::sync::oneshot::Sender<Arc<SessionRegistry>>>,
}

/// [`serve_pinned`]'s body, with its test seams (see [`PinnedSeams`]).
async fn serve_pinned_with(
    opts: ServeOptions,
    backends: ResolvedBackends,
    seams: PinnedSeams,
) -> Result<(), LamboError> {
    // The ledger is the process's, opened pre-lease (J4); each session's
    // lines go through `ledger.for_session(id)`, and each session gets its
    // own `startup` line.
    let ledger = opts.ledger.as_ref().map(|path| Ledger::open(path.clone()));
    if let Some(ledger) = &ledger {
        for id in &opts.sessions {
            ledger.append(&crate::ledger::startup_line(id, &opts.agent, "http"));
        }
    }
    let keep_warm = backends.keep_warm_interval();
    let calibration = EmbedderCalibration::new();
    let early = seams.early.unwrap_or_else(EarlyShutdown::unarmed);
    let store_cfg = backends.store_cfg.clone();
    // The template every session is cloned from: no endpoint (each session
    // derives its own), the unscoped ledger (each session scopes its own).
    let template = serve_builder(
        &opts,
        backends,
        None,
        ledger.clone(),
        early.clone(),
        Some(calibration.clone()),
    );
    let embedder = template.shared_embedder().ok_or_else(|| {
        LamboError::Config("serve: the resolved backends carry no embedder".into())
    })?;
    let registry = SessionRegistry::new(
        opts.sessions.clone(),
        Some(opts.session.clone()),
        LeaseLossPolicy::DetachSession,
        Some(SessionAttacher {
            template,
            store_cfg,
            ledger: ledger.clone(),
            max_sessions: opts.max_sessions,
            agent: opts.agent.clone(),
        }),
        early.clone(),
    );

    // The pinned acquires, in order. Every handle stays held here too, so
    // the last ones drop when `serve_pinned` returns, after the watchdog is
    // disarmed, as a one-session serve's does.
    let mut acquired: Vec<(Arc<Memory>, Option<hub::SessionEndpoint>)> = Vec::new();
    for id in &opts.sessions {
        match registry.acquire(id).await {
            Ok(Acquired::Attached(mem, endpoint)) => acquired.push((mem, endpoint)),
            Ok(Acquired::Held(held)) => registry.mark_held(id, &held).await,
            Err(e) => {
                // Fail closed, but release what this start already took:
                // a refused start must not hold leases until they lapse.
                for (mem, _) in &acquired {
                    if let Err(close) = close_bounded(mem, &early).await {
                        tracing::error!(
                            session = %mem.session(),
                            error = %close,
                            "lambo serve: closing a session after a failed start"
                        );
                    }
                }
                tracing::error!(
                    session = %id,
                    error = %e,
                    "lambo serve: a pinned session could not be attached; refusing to start"
                );
                close_ledger(ledger);
                return Err(e);
            }
        }
    }
    let mems: Vec<Arc<Memory>> = acquired.iter().map(|(mem, _)| Arc::clone(mem)).collect();

    let progress = ShutdownProgress::with_production_watchdog();
    let _disarm = progress.disarm_on_drop();
    let shutdown = registry_shutdown(early.clone(), progress.clone());
    tokio::pin!(shutdown);

    let agent = crate::types::AgentId::new(&opts.agent);
    let tasks = ProcessTasks::spawn(
        &registry,
        &ledger,
        opts.ledger_heartbeat,
        keep_warm,
        &embedder,
        &agent,
        &calibration,
    );
    drop(embedder);
    for (mem, endpoint) in acquired {
        let session = registry.admit(mem, endpoint);
        tracing::info!(
            session = %session.id(),
            agent = %opts.agent,
            transport = ?opts.transport,
            "lambo serve: session attached"
        );
    }
    registry.mark_started();
    registry.spawn_retry_loop();
    tracing::info!(
        sessions = ?opts.sessions,
        default = %opts.session,
        "lambo serve: serving {} pinned sessions",
        opts.sessions.len()
    );
    if let Some(tx) = seams.registry {
        let _ = tx.send(Arc::clone(&registry));
    }

    let transport = serve_http(registry.clone(), &opts, shutdown.as_mut());
    // `mems` drops after `_disarm`, when this returns (declared before it).
    let _held = mems;
    close_holder(&registry, transport, tasks, &early, &progress, ledger).await
}

/// The holder's shutdown, in the order `shutdown`'s stage table names, over
/// whatever the registry holds when the transport ends.
///
/// Stages 1-2: the transport drains, then the keep-warm and the calibration
/// probe stop (#13, #32 PR 3). Stage 3: the registry's attached set is taken
/// (no attach starts after it) and closed concurrently, beside any detach
/// still in flight; stage 4 aborts the event pumps. The transport's error
/// wins, else the first close error (`SessionCloses::report`). Stage 5: the
/// process tasks, the registry's retry loop and each session's watcher.
/// Stage 6: every session's endpoint. Stage 7: the ledger.
async fn close_holder(
    registry: &Arc<SessionRegistry>,
    transport: impl std::future::Future<Output = Result<(), LamboError>>,
    tasks: ProcessTasks,
    early: &EarlyShutdown,
    progress: &ShutdownProgress,
    ledger: Option<Arc<Ledger>>,
) -> Result<(), LamboError> {
    let outcome = stop_transport(transport, || tasks.stop_before_close(), progress).await;
    let sessions = registry.close_set().await;
    let closing: Vec<_> = sessions.iter().map(|s| s.closing()).collect();
    let (closed, ()) = tokio::join!(
        close_sessions(&closing, early, progress),
        registry.join_detaches()
    );
    drop(closing);
    let outcome = match outcome {
        Err(e) => Err(e),
        Ok(()) => closed.report(),
    };
    // Stage 5: heartbeat, keep-warm (again), refusal poller, calibration
    // probe (again); the registry's retry loop and lease watchers.
    progress.run(Stage::BackgroundTasks, || {
        tasks.stop();
        registry.stop_tasks(&sessions);
    });
    // Stage 6: every session's endpoint, after the closes; see
    // `AttachedSession::release_endpoint`.
    progress.begin(Stage::EndpointRelease);
    join_all(
        sessions
            .iter()
            .map(|session| session.release_endpoint())
            .collect(),
    )
    .await;
    // The set's handles (each session's server and `Memory` clone) drop
    // here, where they always have: at the end of stage 6.
    drop(sessions);
    progress.end(Stage::EndpointRelease);
    // Stage 7: the call ledger drains last.
    progress.run(Stage::LedgerClose, || close_ledger(ledger));
    progress.complete();

    outcome
}

#[cfg(test)]
mod tests;
