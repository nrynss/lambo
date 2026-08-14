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
//!   one from the store's side. The operator override is to clear the row by
//!   hand; see [`OPERATOR_OVERRIDE`].
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
/// store blip) before its lease can lapse, while a genuinely crashed holder's
/// lease still expires within one full TTL. Refresh is the heartbeat: a live
/// process keeps its lease; a dead one lets it go.
pub const LEASE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// The operator override for a wedged-but-heartbeating squatter (documented, not
/// automated — see the module docs on why there is no auto-preemption).
///
/// A hung holder whose heartbeat task is still alive keeps refreshing the lease,
/// so no other writer can take the session until the row is cleared by hand.
/// The manual escape is a single DELETE against the durable store, which the
/// next `acquire_lease` then wins:
///
/// ```sql
/// DELETE FROM session_leases WHERE session_id = '<session>';
/// ```
///
/// This is intentionally a destructive, deliberate act: it says "I have
/// confirmed the current holder is not making progress and I am forcing a
/// takeover." The new writer still replays from durable state, so the wedged
/// holder's un-flushed tail is lost exactly as it would be on any crash.
pub const OPERATOR_OVERRIDE: &str = "DELETE FROM session_leases WHERE session_id = '<session>';";

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
        }
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

/// A lease row's identity and timing, as the store reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseInfo {
    /// The holder token ([`LeaseHolder::token`]) currently written to the row.
    pub holder: String,
    /// When this holder first took the lease (stable across its own refreshes).
    pub acquired_at: DateTime<Utc>,
    /// When the lease lapses if not refreshed.
    pub expires_at: DateTime<Utc>,
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
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !h.is_empty() {
                return h;
            }
        }
    }
    "unknown-host".to_string()
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
        };
        assert_eq!(h.token(), "agent-a@host-7#4213");
        // Stable: the same holder always produces the same token (refresh /
        // release depend on it).
        assert_eq!(h.token(), h.clone().token());
    }

    #[test]
    fn for_this_process_uses_the_real_pid() {
        let h = LeaseHolder::for_this_process(&AgentId::new("a"));
        assert_eq!(h.pid, std::process::id());
        assert!(!h.host.is_empty());
    }
}
