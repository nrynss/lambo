//! `lambo serve` — process lifecycle for the MCP server.
//!
//! One process owns one session (spec §2.2). This module builds **one**
//! [`Memory`] from **one** [`ResolvedBackends`], serves it over stdio or
//! streamable HTTP, and guarantees [`Memory::close`] runs on the way out so the
//! final flush happens.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ledger::Ledger;
use crate::mcp::endpoint::SessionEndpoint;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;
use crate::resolve::ResolvedBackends;
use crate::types::LamboError;

mod builder;
mod heartbeat;
mod http_guards;
mod hub;
mod roles;
mod shutdown;
mod signals;
mod transport;

pub use builder::{build_memory, resolve_serve_backends};
pub use heartbeat::authorize_ledger;

use builder::{explain_startup_failure, serve_builder};
use heartbeat::{heartbeat_loop, log_events, record_refused_takeovers, serve_startup_line};
use http_guards::authorize_bind;
pub use http_guards::{
    resolve_auth_token, SecretToken, AUTH_TOKEN_ENV, DEFAULT_MAX_SESSIONS, DEFAULT_RATE_LIMIT_RPS,
};
use hub::serve_endpoint;
use roles::{resolve_role, Role};
use shutdown::holder_shutdown;
use signals::shutdown_signal;
use transport::{serve_http, serve_stdio};

pub(crate) use shutdown::run_and_close;
pub(crate) use signals::EarlyShutdown;
// The crate paths the `memory` and `writeq` tests name; nothing outside a
// test build reads them through `serve`.
#[allow(unused_imports)]
pub(crate) use shutdown::{close_bounded_until, CLOSE_FLUSH_GRACE};

#[allow(unused_imports)] // rustdoc links only
use crate::store::lease;
#[allow(unused_imports)] // rustdoc links only
use roles::ELECTION_BUDGET;

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
    /// Session this process owns.
    pub session: String,
    /// Agent identity this process writes as. See the attribution note on
    /// [`LamboServer`] — `Memory` binds one agent per session handle.
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
        Self {
            session: session.into(),
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
/// *not* released and instead lapses at [`lease::LEASE_TTL`], exactly as it would
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
    let endpoint = SessionEndpoint::for_store(&opts.session, &backends.store_cfg);

    // J4 — the ledger opens HERE, before the acquire attempt inside
    // `resolve_role`. That is the whole of J4's first half: a serve that LOSES
    // the lease exits before it can reach the holder path below, where the
    // ledger used to open, so the acquire itself could not be where a losing
    // serve's story starts. Opening it pre-lease and writing the startup line
    // means a serve about to lose the lease has already left an artifact.
    // `Ledger::open` never fails or blocks the caller, so this is free to move
    // into the pre-lease group; `authorize_ledger` above still refuses the
    // misconfigured `--ledger-heartbeat`-without-`--ledger` pairing first.
    let ledger = opts.ledger.as_ref().map(|path| Ledger::open(path.clone()));
    if let Some(ledger) = &ledger {
        ledger.append(&serve_startup_line(&opts, &endpoint));
    }
    // Issue #13 — read before `serve_builder` consumes the backends. Pure (a
    // config lookup and a type downcast), no I/O, so it belongs in this
    // create-nothing pre-lease group; the task it configures is spawned on the
    // holder path only, below the arming.
    let keep_warm = backends.keep_warm_interval();
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
    );
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
    let shutdown = holder_shutdown(mem.clone(), ledger.clone(), early.clone());
    tokio::pin!(shutdown);

    // I1/I2. `Ledger::open` never fails — a bad path warns once and counts
    // every line as a drop — so nothing here can stop a memory server from
    // serving memory. ONE server handle for the whole process (the HTTP factory
    // clones it), so every transport appends to the same file and the
    // heartbeat's uptime is the session's, not a request's.
    let server = match &ledger {
        Some(ledger) => LamboServer::with_ledger(mem.clone(), Arc::clone(ledger)),
        None => LamboServer::new(mem.clone()),
    };
    let heartbeat = match (&ledger, opts.ledger_heartbeat) {
        (Some(ledger), Some(every)) => {
            tracing::info!(
                path = %ledger.path().display(),
                interval_secs = every.as_secs(),
                version = crate::ledger::VERSION,
                git_sha = crate::ledger::GIT_SHA,
                "lambo serve: call ledger open, heartbeat armed"
            );
            Some(tokio::spawn(heartbeat_loop(
                server.clone(),
                Arc::clone(ledger),
                every,
            )))
        }
        (Some(ledger), None) => {
            tracing::info!(
                path = %ledger.path().display(),
                "lambo serve: call ledger open (no heartbeat)"
            );
            None
        }
        _ => None,
    };
    // Issue #13 — embedder keep-warm. Spawned here, below the arming and
    // beside the heartbeat, for the same reasons: spawning awaits nothing, so
    // the pre-handshake window is not widened, and the loop's first touch is
    // one full interval out, so startup gains no forward. Holder path only: a
    // proxy holds no embedder (it is released when `resolve_role` returns).
    // Aborted when the transport returns, before the close (see
    // `run_and_close`), and again beside the heartbeat after it.
    let keep_warm_task = keep_warm.map(|every| {
        tracing::info!(
            interval_secs = every.as_secs(),
            "lambo serve: embedder keep-warm armed"
        );
        tokio::spawn(crate::embed::keep_warm::keep_warm_loop(
            Arc::clone(mem.embedder()),
            every,
        ))
    });
    // J4 — the holder side of a refused takeover: record the incumbent's
    // line when the store reports a refusal this process turned away. Spawned
    // only when a ledger is attached, and only on the holder path (the proxy
    // branch returned above). Aborted at close like the heartbeat.
    let refusal_poller = match &ledger {
        Some(ledger) => {
            let holder_token =
                crate::store::lease::LeaseHolder::for_this_process(mem.agent()).token();
            Some(tokio::spawn(record_refused_takeovers(
                mem.store().clone(),
                mem.session().clone(),
                mem.agent().clone(),
                holder_token,
                Arc::clone(ledger),
            )))
        }
        None => None,
    };

    // J2 — the session endpoint, bound HERE: below the arming (so a signal
    // during it still reaches `Memory::close`) and below `LamboServer`, which it
    // needs. This is also the first moment the unlink inside `bind` is licensed:
    // we hold the lease, so a socket file already at this path cannot belong to
    // a live holder.
    //
    // **A bind failure does not stop this process serving memory** — the same
    // posture `Ledger::open` takes, for the same reason: reachability is a
    // service to *other* processes, and losing it must not cost this client its
    // memory. The consequence is stated at ERROR rather than swallowed, because
    // the lease row now advertises an address nothing is listening on: a proxy
    // that dials it fails honestly per call (the holder-unreachable path), which
    // is loud but is a real degradation, so the log line names it.
    // No endpoint: a store no second process can see, so there is no hub to be.
    // See `SessionEndpoint::for_store`.
    // JE2E-2: the identity of the socket file this process creates, captured
    // the instant after the bind. It is what licenses the unlink at exit; see
    // `SessionEndpoint::unlink_if_ours` for why the path and the lease are not
    // licences of their own.
    let mut bound_socket: Option<crate::mcp::endpoint::SocketIdentity> = None;
    let hub = match endpoint
        .as_ref()
        .map(|ep| (ep.path().display().to_string(), ep.bind()))
    {
        None => None,
        Some((path, Ok(listener))) => {
            bound_socket = endpoint.as_ref().and_then(|ep| ep.file_identity());
            tracing::info!(
                endpoint = %path,
                "lambo serve: session endpoint bound — other clients on this machine can attach \
                 to this session through it"
            );
            Some(tokio::spawn(serve_endpoint(
                listener,
                server.clone(),
                opts.max_sessions,
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

    // Exactly once, at startup: `events()` is stateful on its first call — it
    // hands out the receiver subscribed *before* the daemon spawned, so the
    // spec §2.5 warm-up condition set (on a resumed session, the whole restored
    // set) is not lost. Draining it here also stops the broadcast channel from
    // filling and lagging the daemon.
    let events = mem.events();
    let event_pump = tokio::spawn(log_events(events));

    tracing::info!(
        session = %opts.session,
        agent = %opts.agent,
        transport = ?opts.transport,
        "lambo serve: session attached"
    );

    let transport = async {
        match opts.transport {
            Transport::Stdio => serve_stdio(server, shutdown.as_mut()).await,
            Transport::Http => serve_http(server, &opts, shutdown.as_mut()).await,
        }
    };

    // Issue #13: the keep-warm stops when the transport does, before the close
    // and its final drain; see `run_and_close`.
    let stop_before_close: Vec<_> = keep_warm_task
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect();
    let outcome = run_and_close(
        mem.clone(),
        transport,
        event_pump,
        &stop_before_close,
        &early,
    )
    .await;

    // After `close()`, deliberately: the tail's durability is the load-bearing
    // guarantee and the ledger is not allowed to be in front of it. The
    // heartbeat is stopped first so it cannot enqueue a line into a ledger
    // that is draining, and `shutdown` is bounded — a writer stuck on a hung
    // filesystem is abandoned, never allowed to hold process exit.
    if let Some(heartbeat) = heartbeat {
        heartbeat.abort();
    }
    // Issue #13. Already aborted inside `run_and_close`, before the close;
    // repeated here (idempotent) so this exit path aborts it without relying
    // on that. Nothing to drain: a touch writes nothing.
    if let Some(task) = keep_warm_task {
        task.abort();
    }
    // J4. The refusal-recorder task is stopped before the ledger drains, so it
    // cannot enqueue a line into a closing ledger.
    if let Some(poller) = refusal_poller {
        poller.abort();
    }
    // J2. Aborted AFTER `close()` (that is where `run_and_close` returned from),
    // deliberately: a proxy's in-flight call must not be cut off before the tail
    // it may have just written is durable. Then the socket file goes, so the
    // next start does not log a stale-socket warning it did not earn.
    if let Some(hub) = hub {
        hub.abort();
    }
    // JE2E-2. Licensed by the socket's own identity, not by the path and not by
    // the lease: this process may be a FENCED ex-holder whose successor is
    // already listening at this address, and even a clean close released the
    // lease a few statements ago. `unlink_if_ours` removes the file only while
    // it is still the inode this process bound.
    if let Some(endpoint) = &endpoint {
        endpoint.unlink_if_ours(bound_socket);
    }
    if let Some(ledger) = ledger {
        ledger.shutdown();
        tracing::info!(
            written = ledger.counters().written(),
            dropped = ledger.counters().dropped(),
            path = %ledger.path().display(),
            "lambo serve: call ledger closed"
        );
    }

    outcome
}

#[cfg(test)]
mod tests;
