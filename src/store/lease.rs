//! Single-writer lease (spec §2.2, T8.6) — the store-enforced half.
//!
//! Spec §2.2 is "one writer per session". Before T8.6 that was **advisory**
//! only: `src/memory.rs`'s process-local `ACTIVE_SESSIONS` registry logs a
//! loud `SecondSessionWriter` ERROR when two `Memory` handles open one session
//! *in this process* — but it cannot see another process or another host, which
//! is exactly where the collisions that silently corrupt a session come from
//! (two `lambo serve` processes on one CockroachDB session, the later flush
//! overwriting the earlier's rows).
//!
//! This module promotes that to **store-enforced**: a per-session lease row the
//! durable store owns, acquired atomically, refused fail-closed when a live one
//! is already held by someone else. Two processes opening the same session now
//! deterministically yield one holder and one honest refusal.
//!
//! ## What this is NOT
//!
//! * **Not durability.** A lease expiring (its holder crashed and stopped
//!   heartbeating) does **not** mean that holder's write-behind tail was
//!   flushed — the tail lived in the crashed process's in-RAM log and died with
//!   it. The new holder must still go through the ordinary startup-load replay
//!   (`store::load`) to pick up whatever *was* made durable; acquiring the lease
//!   proves nothing about the graph's completeness. `Memory::build` already does
//!   that load unconditionally, and a comment there pins the reasoning.
//! * **Not preemption.** A wedged-but-*alive* holder keeps its heartbeat task
//!   running and so keeps the lease indefinitely. There is deliberately no
//!   automatic takeover — a live heartbeat is indistinguishable from a healthy
//!   one from the store's side. The operator override is to expire the row by
//!   hand; see [`OPERATOR_OVERRIDE`].
//!
//!   **A leaked handle is the same shape (T86-5, accepted residual).** The
//!   "a dropped/crashed holder's lease lapses at the TTL" guarantee depends on
//!   `Memory`'s `Drop` actually running — that is what aborts the heartbeat. A
//!   handle kept alive by an `Arc` cycle or `std::mem::forget` never drops, so
//!   its heartbeat refreshes every [`LEASE_HEARTBEAT_INTERVAL`] for the whole
//!   process lifetime and the session stays wedged well past one TTL. This is an
//!   abnormal-leak edge, not a normal path, and there is no cheap store-side
//!   guard (a live refresh is a live refresh); the escape is the same
//!   [`OPERATOR_OVERRIDE`] that expires any wedged-but-heartbeating holder.
//!
//! ## Clock discipline (spec §6.4 / P6 review F18)
//!
//! Lease timestamps are **never** a client argument. `acquire`/`refresh` take a
//! TTL *duration* and each backend stamps `acquired_at` / `expires_at` from its
//! own clock — Cockroach's `now()` (the authority two processes actually share),
//! SQLite's `strftime(...,'now')`, MemoryStore's process `Utc::now()`. The TTL is
//! a relative offset applied to that store clock, so no caller-supplied absolute
//! instant ever reaches a lease row. (A duration is not a timestamp: it cannot
//! backdate anything, which is the F18 hazard.) The lease adds **no wire-visible
//! field** — it is invisible to the MCP surface — so the F18 golden-allowlist
//! guard is untouched.
//!
//! ## Monotonic fencing tokens (GitHub issue #1) — the store is the authority
//!
//! The cooperative fence alone has a **detection window**: between a lease
//! expiring and the old holder's next heartbeat observing the loss (≤ one
//! `LEASE_HEARTBEAT_INTERVAL`) — plus any `FLUSH_ATTEMPT_TIMEOUT`-bounded
//! in-flight flush — the old holder can still write. This module closes that
//! at the **store**: every `session_leases` row carries a strictly-increasing
//! `current_token`, minted on *takeover* (fresh acquire or expired-lease steal)
//! as `max(current, 0) + 1` and **preserved** on a same-holder refresh. The
//! holder carries that token and presents it on every durable write
//! ([`crate::store::GraphStore::flush`] and
//! [`crate::store::GraphStore::record_canonization`]); the store rejects any
//! write whose token is below the row's current one. The cooperative
//! [`crate::memory`] fence stays as belt-and-braces; the token is the hard
//! guarantee, and it closes the window because a pre-takeover holder's stale
//! token can never pass.
//!
//! Only the two durable write gates validate the token. Bulk snapshot writes
//! (`seed()`, fixture parity) are off-lease and deliberately bypass it — see
//! [`lease_permits_write`].
//!
//! ## The token outlives every holder (#23 review H2)
//!
//! A session's `current_token` only ever goes up, for the whole life of the
//! session id. Nothing deletes a lease row or resets its token: a clean
//! release *expires* the row (holder [`RELEASED_HOLDER`], `expires_at` = the
//! store clock, `endpoint` NULL) and keeps `current_token`, and so does the
//! [`OPERATOR_OVERRIDE`]. The next acquire therefore takes the expired-row arm
//! and mints `current + 1`. Before this a release deleted the row, the next
//! acquire minted 1 again, and a lapsed writer still holding an older token
//! passed `presented >= current` against the new holder.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::types::AgentId;

/// How long an acquired lease stays valid without a heartbeat refresh.
///
/// **45s is chosen against the serve shutdown budget, not at random.**
/// `crate::mcp::serve::SHUTDOWN_BUDGET` is 15s (a 5s transport grace + a 10s
/// final-flush grace), the worst-case wall-clock a *graceful* close can take.
/// The TTL must comfortably exceed that so a slow-but-graceful close still holds
/// a valid lease at the moment it calls `release_lease` — it releases cleanly
/// and hands off, rather than letting the lease expire mid-shutdown (which would
/// briefly let a second writer in *while the first is still flushing its tail*).
/// 45s is 3× the budget. A build-time assertion in `serve.rs` pins
/// `LEASE_TTL > SHUTDOWN_BUDGET` so a later bump to either window cannot silently
/// invert the relationship.
pub const LEASE_TTL: Duration = Duration::from_secs(45);

/// How often a live holder refreshes its lease — one third of [`LEASE_TTL`].
///
/// A third means a holder survives two consecutive missed refreshes (a transient
/// store blip), while a genuinely crashed holder's lease still expires within
/// one full TTL. Precisely (J2-R3-4): the third attempt lands *at* expiry, not
/// before it, so survival there is not the row still being valid — it is the
/// holder re-acquiring a just-lapsed row that nothing else has contended for.
/// Refresh is the heartbeat: a live process keeps its lease; a dead one lets it
/// go.
pub const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// How long a `lease_refusals` row survives before an adapter purges it
/// (JE2E-1).
///
/// **Derived from the widest window any reader ever looks back over.** The only
/// reader is the incumbent holder's recorder task
/// (`mcp::serve::record_refused_takeovers`), which starts at `now − LEASE_TTL`
/// and from there only ever moves its cursor *forward*. So a row older than one
/// [`LEASE_TTL`] cannot be read by any poller that is already running, and the
/// widest a freshly-started holder can look back is exactly one [`LEASE_TTL`]
/// too. The retention has to be strictly above that or the purge would race the
/// read it exists beside.
///
/// One hour, not 45 seconds, because the rows have a second reader the poller
/// does not: an operator asking "why did this client have no memory" runs a
/// `SELECT` against this table minutes or hours later, and a retention pinned
/// to the read window would have swept the answer away before the question was
/// asked. An hour is 80× the read window and still bounds the table on the
/// founding scenario — a client auto-respawning a losing serve once a second
/// keeps 3,600 rows rather than one row per respawn forever.
///
/// Build-guarded against the read window below, so a later change to either
/// cannot silently invert the relationship.
pub const LEASE_REFUSAL_RETENTION: Duration = Duration::from_secs(3600);

const _: () = assert!(
    LEASE_REFUSAL_RETENTION.as_secs() > LEASE_TTL.as_secs(),
    "lease_refusals must be retained for longer than the widest window a holder's recorder \
     task reads back (one LEASE_TTL), or the purge would delete rows the poller is about to read"
);

/// The operator override for a wedged-but-heartbeating squatter (documented, not
/// automated — see the module docs on why there is no auto-preemption).
///
/// A hung holder whose heartbeat task is still alive keeps refreshing the lease,
/// so no other writer can take the session until the row is expired by hand.
/// The manual escape is a single UPDATE against the durable store, which the
/// next `acquire_lease` then wins:
///
/// ```sql
/// UPDATE session_leases SET holder = 'lambo:released', expires_at = acquired_at,
///   endpoint = NULL WHERE session_id = '<session>' AND holder <> 'lambo:erased';
/// ```
///
/// An UPDATE, never a DELETE: it keeps `current_token`, so the next acquire
/// mints a token above every one the wedged holder (or any earlier writer)
/// still holds, and their later writes are refused (#23 review H2). Deleting
/// the row would restart the session at token 1 and let them through.
/// `expires_at = acquired_at` is in the past on every store and in every
/// timestamp representation, so the statement is the same for SQLite and the
/// Postgres family. The `holder <> 'lambo:erased'` guard keeps it from
/// lifting an erasure tombstone (see `store::erase`).
///
/// This is intentionally a deliberate act: it says "I have confirmed the
/// current holder is not making progress and I am forcing a takeover." The new
/// writer still replays from durable state, so the wedged holder's un-flushed
/// tail is lost exactly as it would be on any crash.
pub const OPERATOR_OVERRIDE: &str = "UPDATE session_leases SET holder = 'lambo:released', \
     expires_at = acquired_at, endpoint = NULL \
     WHERE session_id = '<session>' AND holder <> 'lambo:erased';";

/// The `session_leases.holder` of a released row (#23 review H2).
///
/// A release (and the [`OPERATOR_OVERRIDE`]) expires the row instead of
/// deleting it, so the fencing token survives, and hands it to this holder so
/// the next acquire is a takeover for everyone, the releasing identity
/// included: a refresh keeps the token, a takeover mints a new one. Never
/// equal to a live holder's token, which always carries `@` and `#`.
pub const RELEASED_HOLDER: &str = "lambo:released";

/// `true` when a lease row's holder is the released marker.
pub fn is_released_holder(holder: &str) -> bool {
    holder == RELEASED_HOLDER
}

/// `true` for a holder value the store writes itself and no writer may take
/// as its identity: [`RELEASED_HOLDER`] and the erasure tombstone's
/// [`crate::store::erase::ERASED_HOLDER`].
pub fn is_reserved_holder(holder: &str) -> bool {
    is_released_holder(holder) || crate::store::erase::is_erased_holder(holder)
}

/// Refuse a caller whose holder token is a reserved value (#23 review H1).
///
/// Every acquire, refresh and erase runs this first. A [`LeaseHolder::token`]
/// is `agent@host#pid` and cannot equal either reserved value, so this never
/// fires for a real caller; it is here so that holding the tombstone or the
/// released marker can never be a writer's lease even if the token format
/// changes, which would otherwise let a "refresh" extend a tombstone or a
/// write fence pass as its holder.
pub fn refuse_reserved_holder(holder: &str) -> Result<(), crate::types::StoreError> {
    if is_reserved_holder(holder) {
        return Err(crate::types::StoreError::Invariant(format!(
            "lease holder {holder:?} is reserved for the store's own rows (a released lease or \
             an erased session) and cannot take a lease or erase"
        )));
    }
    Ok(())
}

/// Who holds a session lease — agent id + process id + host.
///
/// This is the human-readable identity an operator sees in a refusal ("held by
/// `agent-a@host-7#4213`"). Two writers in the *same* process share pid+host and
/// are distinguished only by agent id; the same-process, same-agent double-open
/// is therefore **not** caught here (its token is identical, so a second acquire
/// looks like a refresh) — that case is left to the cheap in-process
/// `ACTIVE_SESSIONS` advisory log, which the lease does not replace. The lease's
/// job is the cross-process / cross-host collision, where pid or host differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHolder {
    pub agent: AgentId,
    pub pid: u32,
    pub host: String,
    /// Where this holder can be reached, published into
    /// `session_leases.endpoint` by the acquire (J2). `None` — the default, and
    /// what every CLI writer verb uses — means "not reachable": the row then
    /// says so, and a refused `serve` fails honestly instead of dialling
    /// nothing. Deliberately **not** part of [`LeaseHolder::token`]: the token
    /// is the identity a refresh and a release match on, and it must stay
    /// stable even if a holder's reachability ever changed under it.
    pub endpoint: Option<String>,
}

impl LeaseHolder {
    /// The identity of the current process, writing as `agent`.
    ///
    /// `pid` and `host` come from the OS, never from a caller. `host` is
    /// best-effort (env `HOSTNAME`/`HOST`, then the `hostname` command, then a
    /// placeholder): it only needs to *distinguish* hosts, and within one host
    /// the pid already distinguishes processes, so an imperfect hostname never
    /// weakens same-host enforcement.
    pub fn for_this_process(agent: &AgentId) -> Self {
        Self {
            agent: agent.clone(),
            pid: std::process::id(),
            host: detect_host(),
            endpoint: None,
        }
    }

    /// Publish an endpoint alongside this holder's identity (J2).
    ///
    /// Only [`crate::mcp::serve`](mod@crate::mcp::serve) calls this: a serve process is the only writer
    /// another process can forward tool calls to, so it is the only one whose
    /// reachability is worth recording. A CLI writer holds the lease for the
    /// length of one verb and is not proxyable, which is why the column is
    /// nullable and why the absence is *meaningful* rather than missing data.
    pub fn reachable_at(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// The stable string persisted in the lease row's `holder` column.
    ///
    /// Stable for the whole life of a handle (heartbeat refreshes reuse it, and
    /// release matches on it), so a lease can only ever be refreshed or released
    /// by the exact identity that took it.
    pub fn token(&self) -> String {
        format!("{}@{}#{}", self.agent, self.host, self.pid)
    }
}

impl std::fmt::Display for LeaseHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.token())
    }
}

/// A lease row's identity, timing, fencing token and reachability, as the store
/// reports it. Four things, not three: `endpoint` joined the row in J2 so a
/// writer refused by a live lease can find the holder rather than only be told
/// about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseInfo {
    /// The holder token ([`LeaseHolder::token`]) currently written to the row.
    pub holder: String,
    /// Monotonic fencing token (GitHub issue #1). Minted on takeover (fresh or
    /// expired-lease steal), PRESERVED across a same-holder refresh. The holder
    /// must present this on every durable write; the store rejects any write
    /// whose token is below the row's current one. `0` is the "never minted"
    /// sentinel (no lease has ever been taken on the session). Never reset: a
    /// release keeps it (see the module docs).
    pub token: u64,
    /// When this holder first took the lease (stable across its own refreshes).
    pub acquired_at: DateTime<Utc>,
    /// When the lease lapses if not refreshed.
    pub expires_at: DateTime<Utc>,
    /// Where the holder said it can be reached — `session_leases.endpoint`
    /// (J2), as written by the holder's own acquire. `None` means the holder
    /// published no endpoint: either a pre-J2 row, or a writer that is not a
    /// `serve` process. A refused `serve` treats `None` as "no hub here" and
    /// fails honestly rather than guessing an address.
    pub endpoint: Option<String>,
}

/// Store-side fence check (GitHub issue #1): may a write presenting `presented`
/// proceed against a lease row whose current fencing token is `current`?
///
/// * `current == 0` (the "never minted" sentinel — the session has no lease) →
///   any write is allowed. This is the `seed()` / fixture-parity bypass: a
///   full-snapshot write, off-lease, must not fail.
/// * `current > 0` (the session IS leased) → the writer must present
///   `Some(token)` with `token >= current`. A pre-takeover holder from an
///   earlier generation presents a strictly lower token and is rejected; `None`
///   (no token at all) is rejected too — a lease has been set, so a write that
///   does not even claim a token must not slip through.
pub fn lease_permits_write(current: u64, presented: Option<u64>) -> bool {
    if current == 0 {
        return true;
    }
    presented.is_some_and(|p| p >= current)
}

/// Outcome of an [`crate::store::GraphStore::acquire_lease`] /
/// [`crate::store::GraphStore::refresh_lease`] attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// The lease is ours — freshly taken, an expired one reclaimed, or our own
    /// refreshed. Carries the row as written.
    Acquired(LeaseInfo),
    /// Refused: a *live* lease is held by someone else. Fail closed.
    Held {
        /// The current holder's row.
        current: LeaseInfo,
        /// How long the current holder has held it (store clock − `acquired_at`),
        /// clamped to zero if the clocks disagree slightly.
        age: Duration,
    },
}

impl LeaseOutcome {
    /// `true` for [`LeaseOutcome::Acquired`].
    pub fn is_acquired(&self) -> bool {
        matches!(self, LeaseOutcome::Acquired(_))
    }
}

/// A recorded lease refusal (J4): a writer attempted to take a session's
/// single-writer lease and the incumbent holder refused it.
///
/// This is the durable fact "from both sides" names: a refused acquisition
/// produces a **loser-side** ledger line (written by the refused writer, which
/// is alive on this path) and a **holder-side** ledger line (written by the
/// incumbent once it learns of the refusal through the store). [`LeaseRefusal`]
/// is the store-side record that carries the fact from one process to the
/// other. `at` is **stamped by the store's clock** (F18 — never a caller
/// instant), the same discipline every lease timestamp obeys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRefusal {
    /// The session the refusal was about.
    pub session: crate::types::SessionId,
    /// Store-clock instant of the refusal.
    pub at: DateTime<Utc>,
    /// The refused writer's holder token ([`LeaseHolder::token`]).
    pub refused_by: String,
    /// The incumbent holder's token, from the lease row at refusal time.
    pub current_holder: String,
}

/// Best-effort host name, dependency-free. Only needs to distinguish hosts (see
/// [`LeaseHolder::for_this_process`]).
fn detect_host() -> String {
    for var in ["HOSTNAME", "HOST"] {
        if let Ok(h) = std::env::var(var) {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    "unknown-host".to_string()
}

/// Store-agnostic lease checks every adapter runs against itself: the SQLite
/// and `MemoryStore` unit tests call them directly, the Postgres family from
/// its live legs (`store::pg::release_fencing`).
#[cfg(test)]
pub(crate) mod testkit {
    use std::time::Duration;

    use chrono::Utc;

    use super::{LeaseHolder, LeaseOutcome};
    use crate::store::GraphStore;
    use crate::types::{
        AgentId, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId, StoreError,
    };

    pub(crate) fn holder(agent: &str, pid: u32) -> LeaseHolder {
        LeaseHolder {
            endpoint: None,
            agent: AgentId::new(agent),
            pid,
            host: "lease-testkit".into(),
        }
    }

    /// One interaction in `sid`: the smallest batch that creates the session.
    pub(crate) fn interaction_batch(sid: &SessionId, text: &str) -> MutationBatch {
        MutationBatch {
            mutations: vec![Mutation::UpsertNode {
                node: Node::Interaction(Interaction {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    agent_id: AgentId::new("lease-testkit"),
                    prompt_text: Some(text.into()),
                    previous_id: None,
                    created_at: Utc::now(),
                    event_time: None,
                }),
            }],
            ..Default::default()
        }
    }

    async fn acquired(
        store: &dyn GraphStore,
        sid: &SessionId,
        who: &LeaseHolder,
        ttl: Duration,
    ) -> u64 {
        match store.acquire_lease(sid, who, ttl).await.expect("acquire") {
            LeaseOutcome::Acquired(info) => info.token,
            other => panic!("{who} must take {sid}: {other:?}"),
        }
    }

    fn assert_stale(res: Result<(), StoreError>, what: &str) {
        match res {
            Err(StoreError::StaleWrite(_)) => {}
            other => panic!("{what} must be refused as a stale write, got {other:?}"),
        }
    }

    /// A release keeps the session's fencing token (H2 of the #23 review).
    ///
    /// Before the fix a release deleted the lease row, so the next acquire
    /// minted token 1 again and any writer still holding an older token passed
    /// the `presented >= current` fence against the new holder. Two shapes:
    ///
    /// 1. acquire (t1), release, another writer acquires (t2): a write
    ///    presenting t1, or no token, is refused, and t2 is above t1;
    /// 2. X takes the session and its lease lapses, Y takes it over and closes
    ///    cleanly, Z acquires: zombie X's write is refused and Z's lands.
    ///
    /// `sid` and `sid2` must be fresh ids (the pg legs share a cluster).
    pub(crate) async fn check_release_keeps_the_fencing_token(
        store: &dyn GraphStore,
        sid: &SessionId,
        sid2: &SessionId,
    ) {
        let long = Duration::from_secs(60);

        // Shape 1.
        let a = holder("release-a", 1);
        let b = holder("release-b", 2);
        let t1 = acquired(store, sid, &a, long).await;
        store
            .flush(&interaction_batch(sid, "a's write"), Some(t1))
            .await
            .expect("a writes under its own lease");
        store.release_lease(sid, &a).await.expect("release");
        let t2 = acquired(store, sid, &b, long).await;
        assert!(
            t2 > t1,
            "a token minted after a release must be above every earlier one ({t1} then {t2})"
        );
        assert_stale(
            store
                .flush(&interaction_batch(sid, "a, after release"), Some(t1))
                .await,
            "a write presenting the released token",
        );
        assert_stale(
            store.flush(&interaction_batch(sid, "no token"), None).await,
            "an unleased write to a session that has been leased",
        );
        store
            .flush(&interaction_batch(sid, "b's write"), Some(t2))
            .await
            .expect("the new holder writes");
        store.release_lease(sid, &b).await.expect("release b");
        // The same identity coming back after its own release is a takeover
        // too, not a refresh of the released row.
        let t3 = acquired(store, sid, &a, long).await;
        assert!(
            t3 > t2,
            "re-acquire after release must mint ({t2} then {t3})"
        );
        store.release_lease(sid, &a).await.expect("release a");
        let row = store
            .read_lease(sid)
            .await
            .expect("read")
            .expect("row kept");
        assert_eq!(row.token, t3, "a release keeps current_token");
        assert_eq!(row.endpoint, None, "a released row publishes no endpoint");
        assert!(row.expires_at <= Utc::now(), "a released row is expired");

        // Shape 2: the zombie.
        let x = holder("zombie-x", 3);
        let y = holder("cli-y", 4);
        let z = holder("next-z", 5);
        let tx = acquired(store, sid2, &x, Duration::from_secs(1)).await;
        store
            .flush(&interaction_batch(sid2, "x's write"), Some(tx))
            .await
            .expect("x writes");
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        let ty = acquired(store, sid2, &y, long).await;
        store
            .flush(&interaction_batch(sid2, "y's write"), Some(ty))
            .await
            .expect("y writes");
        store
            .release_lease(sid2, &y)
            .await
            .expect("y closes cleanly");
        let tz = acquired(store, sid2, &z, long).await;
        assert!(tz > ty && ty > tx, "tokens are monotonic: {tx} {ty} {tz}");
        assert_stale(
            store
                .flush(&interaction_batch(sid2, "zombie x"), Some(tx))
                .await,
            "zombie x's flush after y released and z acquired",
        );
        store
            .flush(&interaction_batch(sid2, "z's write"), Some(tz))
            .await
            .expect("z writes");
        let snap = store.load_session(sid2).await.expect("load");
        let texts: Vec<_> = snap
            .interactions
            .iter()
            .filter_map(|i| i.prompt_text.clone())
            .collect();
        assert!(
            !texts.iter().any(|t| t == "zombie x"),
            "the zombie's write must not land: {texts:?}"
        );
        store.release_lease(sid2, &z).await.expect("release z");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_exceeds_heartbeat_so_a_missed_beat_is_survivable() {
        assert!(
            LEASE_HEARTBEAT_INTERVAL < LEASE_TTL,
            "a lease that expires before its first refresh could never be kept alive"
        );
        // A third of the TTL: two missed beats are survivable, a crash still
        // expires within one TTL.
        assert!(LEASE_HEARTBEAT_INTERVAL * 2 < LEASE_TTL);
    }

    #[test]
    fn holder_token_is_stable_and_names_all_three_parts() {
        let h = LeaseHolder {
            agent: AgentId::new("agent-a"),
            pid: 4213,
            host: "host-7".into(),
            endpoint: None,
        };
        assert_eq!(h.token(), "agent-a@host-7#4213");
        // J2: an endpoint is reachability, not identity — the token a refresh
        // and a release match on must not move when one is published.
        assert_eq!(h.clone().reachable_at("/tmp/x.sock").token(), h.token());
        // Stable: the same holder always produces the same token (refresh /
        // release depend on it).
        assert_eq!(h.token(), h.clone().token());
    }

    #[test]
    fn the_reserved_holders_are_refused_and_no_real_token_is_one() {
        for reserved in [RELEASED_HOLDER, crate::store::erase::ERASED_HOLDER] {
            assert!(is_reserved_holder(reserved));
            assert!(matches!(
                refuse_reserved_holder(reserved),
                Err(crate::types::StoreError::Invariant(_))
            ));
            let lookalike = LeaseHolder {
                agent: AgentId::new(reserved),
                pid: 0,
                host: reserved.into(),
                endpoint: None,
            };
            assert!(refuse_reserved_holder(&lookalike.token()).is_ok());
        }
    }

    #[test]
    fn for_this_process_uses_the_real_pid() {
        let h = LeaseHolder::for_this_process(&AgentId::new("a"));
        assert_eq!(h.pid, std::process::id());
        assert!(!h.host.is_empty());
    }
}
