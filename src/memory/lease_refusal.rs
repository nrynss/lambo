//! The operator-facing refusal for a session whose single-writer lease another
//! writer holds (#118).
//!
//! A lease row says who took the lease, when, and when it lapses. It does not
//! say whether the holder is alive: a holder that was `kill -9`'d leaves a row
//! that stays unexpired for up to [`LEASE_TTL`]. So the refusal reports the
//! row's facts (when it was last refreshed, when it expires) and claims a holder
//! is gone only on better evidence than the row:
//!
//! * the holder is on this host and its pid no longer exists (`kill(pid, 0)`
//!   answers `ESRCH`), checked here; or
//! * `serve`'s election probed the holder's endpoint and the connect was
//!   refused, which the caller passes in.
//!
//! Both are advisory. A pid can be reused and an endpoint can be slow, so
//! neither changes what the lease does: the refusal stands until the lease
//! expires or an operator expires it by hand. Nothing here takes over.

use std::time::Duration;

use crate::store::lease::{detect_host, holder_host_and_pid, LeaseInfo, LEASE_TTL};
use crate::types::SessionId;

/// Where the operator override is documented, quoted in every variant.
const TAKEOVER_NOTE: &str = "see the single-writer lease note in docs/reference/cli.mdx";

/// What the refusing process knows about the holder beyond the lease row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HolderEvidence {
    /// Only the row. The holder may be alive, or dead with its lease not yet
    /// expired; the message says neither.
    RowOnly,
    /// The holder is on this host and `pid` is not a running process.
    ProcessGone { pid: u32 },
    /// `serve`'s election found the holder's endpoint refusing connections.
    EndpointRefused,
}

/// The lease row's timing, in whole seconds of the store's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LeaseTiming {
    /// Since the holder first took the lease.
    pub(crate) held_for: u64,
    /// Since the holder last refreshed it, when that can be told from the row.
    pub(crate) refreshed_ago: Option<u64>,
    /// Until the lease lapses if it is not refreshed again.
    pub(crate) expires_in: u64,
}

impl LeaseTiming {
    /// Read the timing off a refused acquire's row.
    ///
    /// `age` is the store's clock minus `acquired_at` (what
    /// [`crate::store::lease::LeaseOutcome::Held`] carries), so the store's
    /// "now" is `acquired_at + age`, and every figure here is on the store's
    /// clock rather than this host's. `refreshed_ago` assumes the holder
    /// refreshes with [`LEASE_TTL`], which every writer does; a row whose
    /// remaining time exceeds it was written with some other TTL, and then the
    /// figure is left out rather than guessed.
    pub(crate) fn from_row(current: &LeaseInfo, age: Duration) -> Self {
        let store_now = current.acquired_at
            + chrono::Duration::from_std(age).unwrap_or_else(|_| chrono::Duration::zero());
        let expires_in = (current.expires_at - store_now)
            .to_std()
            .unwrap_or(Duration::ZERO);
        let refreshed_ago = LEASE_TTL
            .checked_sub(expires_in)
            .map(|since| since.as_secs());
        Self {
            held_for: age.as_secs(),
            refreshed_ago,
            expires_in: expires_in.as_secs(),
        }
    }
}

/// The refusal for a lease held by `current`, with this host's own evidence
/// about the holder folded in. `endpoint_refused` is `serve`'s probe outcome;
/// every other caller passes `false`.
pub(crate) fn lease_held_message(
    session: &SessionId,
    current: &LeaseInfo,
    age: Duration,
    endpoint_refused: bool,
) -> String {
    let evidence = match local_holder_gone(&current.holder) {
        Some(pid) => HolderEvidence::ProcessGone { pid },
        None if endpoint_refused => HolderEvidence::EndpointRefused,
        None => HolderEvidence::RowOnly,
    };
    format_lease_held(
        session,
        &current.holder,
        LeaseTiming::from_row(current, age),
        evidence,
    )
}

/// Compose the refusal from facts already gathered. Pure, so every branch is
/// testable without a store or a dead process.
pub(crate) fn format_lease_held(
    session: &SessionId,
    holder: &str,
    timing: LeaseTiming,
    evidence: HolderEvidence,
) -> String {
    let LeaseTiming {
        held_for,
        refreshed_ago,
        expires_in,
    } = timing;
    let opening = format!(
        "session {session} is already held by another writer ({holder}) — it acquired the \
         single-writer lease {held_for}s ago"
    );
    let refreshed = match refreshed_ago {
        Some(r) => format!("last refreshed {r}s ago"),
        None => "last refresh time unknown".to_string(),
    };
    match evidence {
        HolderEvidence::RowOnly => format!(
            "{opening}; the lease was {refreshed} and expires in {expires_in}s unless the holder \
             refreshes it again. Refusing to open a second writer. If that holder is wedged, an \
             operator can force a takeover ({TAKEOVER_NOTE})"
        ),
        HolderEvidence::ProcessGone { pid } => format!(
            "{opening}, but the holder process is gone: no process {pid} exists on this host. \
             Its lease ({refreshed}) expires in {expires_in}s. Refusing to open a second writer \
             until it does; an operator can force a takeover now instead ({TAKEOVER_NOTE})"
        ),
        HolderEvidence::EndpointRefused => format!(
            "{opening}; the lease was {refreshed} and expires in {expires_in}s, but the holder's \
             endpoint is not answering, so it has most likely died. Refusing to open a second \
             writer. If that holder is wedged, an operator can force a takeover \
             ({TAKEOVER_NOTE})"
        ),
    }
}

/// The holder's pid, when the holder token names this host and no process
/// with that pid exists. `None` whenever that cannot be established: another
/// host, a token in another shape, this very process, or a pid that exists
/// (including one this process may not signal).
fn local_holder_gone(token: &str) -> Option<u32> {
    let (host, pid) = holder_host_and_pid(token)?;
    if pid == std::process::id() || host != detect_host() {
        return None;
    }
    process_is_gone(pid).then_some(pid)
}

/// `true` only when the OS says no process has this pid.
#[cfg(unix)]
fn process_is_gone(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        // 0 and negatives address process groups, not a process.
        return false;
    }
    // SAFETY: signal 0 delivers nothing; `kill` only checks that the pid
    // exists and may be signalled, and touches no memory the caller owns.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// No liveness check off unix: the message falls back to the row's facts.
#[cfg(not(unix))]
fn process_is_gone(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> LeaseTiming {
        LeaseTiming {
            held_for: 77,
            refreshed_ago: Some(12),
            expires_in: 33,
        }
    }

    fn session() -> SessionId {
        SessionId::new("s")
    }

    /// #118: a row alone never claims the holder is refreshing, or alive.
    #[test]
    fn a_row_only_refusal_reports_the_lease_timing_and_no_liveness() {
        let msg = format_lease_held(&session(), "a@h#1", timing(), HolderEvidence::RowOnly);
        assert_eq!(
            msg,
            "session s is already held by another writer (a@h#1) — it acquired the \
             single-writer lease 77s ago; the lease was last refreshed 12s ago and expires in \
             33s unless the holder refreshes it again. Refusing to open a second writer. If \
             that holder is wedged, an operator can force a takeover (see the single-writer \
             lease note in docs/reference/cli.mdx)"
        );
        assert!(!msg.contains("still refreshing"), "{msg}");
    }

    #[test]
    fn a_gone_local_holder_is_named_with_its_expiry_and_the_runbook() {
        let msg = format_lease_held(
            &session(),
            "a@h#37916",
            timing(),
            HolderEvidence::ProcessGone { pid: 37916 },
        );
        assert_eq!(
            msg,
            "session s is already held by another writer (a@h#37916) — it acquired the \
             single-writer lease 77s ago, but the holder process is gone: no process 37916 \
             exists on this host. Its lease (last refreshed 12s ago) expires in 33s. Refusing \
             to open a second writer until it does; an operator can force a takeover now \
             instead (see the single-writer lease note in docs/reference/cli.mdx)"
        );
    }

    #[test]
    fn a_refused_endpoint_says_most_likely_died() {
        let msg = format_lease_held(
            &session(),
            "a@h#1",
            timing(),
            HolderEvidence::EndpointRefused,
        );
        assert!(msg.contains("expires in 33s"), "{msg}");
        assert!(msg.contains("endpoint is not answering"), "{msg}");
        assert!(msg.contains("most likely died"), "{msg}");
        assert!(!msg.contains("still refreshing"), "{msg}");
    }

    /// Every branch: a continuation that lost its `\` shows up as a double
    /// space (J2-R1-9).
    #[test]
    fn no_branch_has_a_broken_continuation() {
        for evidence in [
            HolderEvidence::RowOnly,
            HolderEvidence::ProcessGone { pid: 9 },
            HolderEvidence::EndpointRefused,
        ] {
            for refreshed_ago in [Some(12), None] {
                let t = LeaseTiming {
                    refreshed_ago,
                    ..timing()
                };
                let msg = format_lease_held(&session(), "a@h#1", t, evidence);
                assert!(!msg.contains("  "), "{evidence:?}: {msg}");
            }
        }
    }

    #[test]
    fn timing_is_read_on_the_store_clock() {
        let acquired_at = chrono::Utc::now() - chrono::Duration::seconds(100);
        let row = LeaseInfo {
            holder: "a@h#1".into(),
            token: 1,
            acquired_at,
            // Refreshed 10s before "now" (acquired_at + 100s).
            expires_at: acquired_at
                + chrono::Duration::seconds(90)
                + chrono::Duration::from_std(LEASE_TTL).unwrap(),
            endpoint: None,
        };
        let t = LeaseTiming::from_row(&row, Duration::from_secs(100));
        assert_eq!(
            t,
            LeaseTiming {
                held_for: 100,
                refreshed_ago: Some(10),
                expires_in: LEASE_TTL.as_secs() - 10,
            }
        );
        // A row written with a longer TTL than ours: the refresh time is not
        // guessed.
        let long = LeaseInfo {
            expires_at: acquired_at + chrono::Duration::seconds(100 + 3_600),
            ..row
        };
        let t = LeaseTiming::from_row(&long, Duration::from_secs(100));
        assert_eq!(t.refreshed_ago, None);
        assert_eq!(t.expires_in, 3_600);
    }

    #[test]
    fn holder_tokens_split_from_the_right() {
        assert_eq!(holder_host_and_pid("a@h#1"), Some(("h", 1)));
        assert_eq!(
            holder_host_and_pid("we@ird#id@host-7#4213"),
            Some(("host-7", 4213))
        );
        assert_eq!(holder_host_and_pid("lambo:released"), None);
        assert_eq!(holder_host_and_pid("a@h#notapid"), None);
    }

    /// The liveness check, both ways, against real pids.
    #[cfg(unix)]
    #[test]
    fn the_local_liveness_check_tells_a_live_pid_from_a_gone_one() {
        let host = detect_host();
        // This process: never reported gone (it is the caller).
        let me = format!("a@{host}#{}", std::process::id());
        assert_eq!(local_holder_gone(&me), None);
        // A child that has exited and been reaped: its pid is free.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap");
        if process_is_gone(pid) {
            assert_eq!(local_holder_gone(&format!("a@{host}#{pid}")), Some(pid));
            // Same pid on another host: not ours to judge.
            assert_eq!(
                local_holder_gone(&format!("a@{host}-elsewhere#{pid}")),
                None
            );
        }
        // pid 1 exists on every unix (and is not ours to signal): alive.
        assert!(!process_is_gone(1));
    }
}
