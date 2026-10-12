//! The operator-facing refusal for a session whose single-writer lease another
//! writer holds (#118).
//!
//! A lease row says who took the lease, when, and when it lapses. It does not
//! say whether the holder is alive: a holder that was `kill -9`'d leaves a row
//! that stays unexpired for up to [`LEASE_TTL`]. So the refusal reports the
//! row's facts (when it was last refreshed, when it expires) and claims a holder
//! is gone only on better evidence than the row:
//!
//! * the holder names this host and its pid is not visible from here
//!   (`kill(pid, 0)` answers `ESRCH`), checked here; or
//! * `serve`'s election probed the holder's endpoint and the connect was
//!   refused, which the caller passes in.
//!
//! Both are advisory. A pid can be reused, a host name is not a PID namespace
//! (two containers on `--network host`, or a Kubernetes `hostNetwork` pod,
//! share the name and not the pids), and an endpoint can be slow, so the
//! message hedges what it infers. The one exception (#126): when the holder's
//! token carries a boot id and a PID-namespace id and both equal this
//! process's, a pid that is not visible is a process that has exited, and the
//! message says so plainly. Neither changes what the lease does: the refusal
//! stands until the lease expires or an operator expires it by hand. Nothing
//! here takes over.

use std::time::Duration;

use crate::store::lease::{
    detect_host, parse_holder, process_instance, LeaseInfo, ProcessInstance, LEASE_TTL,
    PLATFORM_HAS_PID_NAMESPACES, UNKNOWN_HOST,
};
use crate::types::SessionId;

/// Where the operator override is documented, quoted in every variant.
const TAKEOVER_NOTE: &str = "see the single-writer lease note in docs/reference/cli.mdx";

/// What the refusing process knows about the holder beyond the lease row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HolderEvidence {
    /// Only the row. The holder may be alive, or dead with its lease not yet
    /// expired; the message says neither.
    RowOnly,
    /// The holder names this host and `pid` is not visible from this
    /// process's PID namespace. `certain` (#126) when the holder's token
    /// carries the same boot id and PID namespace as this process: then the
    /// holder ran where this process runs, and it has exited. Otherwise the
    /// message hedges.
    ProcessGone { pid: u32, certain: bool },
    /// `serve`'s election found the holder's endpoint refusing connections.
    EndpointRefused,
}

/// The lease row's timing, in whole seconds, all measured from one instant:
/// the moment the refused acquire read the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LeaseTiming {
    /// Since the holder first took the lease.
    pub(crate) held_for: u64,
    /// Since the holder last refreshed it, when that can be told from the row.
    /// Never more than `held_for`.
    pub(crate) refreshed_ago: Option<u64>,
    /// Until the lease lapses if it is not refreshed again.
    pub(crate) expires_in: u64,
}

/// How long the lease in `current` has left, measured from the instant `age`
/// was taken.
///
/// `age` is what [`crate::store::lease::LeaseOutcome::Held`] carries: the
/// refusing process's `Utc::now()` at the read minus the row's `acquired_at`
/// (clamped at zero). So `acquired_at + age` is that same instant, and the
/// remaining time, the age and the refresh time all come from one clock
/// reading rather than from separate `now()` calls. The row's timestamps
/// were stamped by the holder's or the store's clock, so skew between the
/// two hosts shifts these figures; nothing here can correct for that.
pub(crate) fn lease_remaining(current: &LeaseInfo, age: Duration) -> Duration {
    let read_at = current.acquired_at
        + chrono::Duration::from_std(age).unwrap_or_else(|_| chrono::Duration::zero());
    (current.expires_at - read_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

impl LeaseTiming {
    /// Read the timing off a refused acquire's row (see [`lease_remaining`]
    /// for the clock). `refreshed_ago` assumes the holder refreshes with
    /// [`LEASE_TTL`], which every writer does; a row whose remaining time
    /// exceeds it was written with some other TTL, and then the figure is
    /// left out rather than guessed. It is clamped to `held_for`: a lease
    /// cannot have been refreshed before it was taken, and skew can make the
    /// arithmetic say so.
    pub(crate) fn from_row(current: &LeaseInfo, age: Duration) -> Self {
        let expires_in = lease_remaining(current, age);
        let held_for = age.as_secs();
        let refreshed_ago = LEASE_TTL
            .checked_sub(expires_in)
            .map(|since| since.as_secs().min(held_for));
        Self {
            held_for,
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
        Some(gone) => gone,
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

/// `true` when the holder names this host and its pid is not visible from
/// here: the same advisory check [`lease_held_message`] reports, for a
/// caller that words its own advice around the refusal.
pub(crate) fn holder_looks_gone(current: &LeaseInfo) -> bool {
    local_holder_gone(&current.holder).is_some()
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
        HolderEvidence::ProcessGone { pid, certain: true } => format!(
            "{opening}, and the holder process is gone: it ran in this boot and PID namespace, and \
             no process {pid} exists. Its lease ({refreshed}) expires in {expires_in}s. Refusing \
             to open a second writer until it does; an operator can force a takeover now instead \
             ({TAKEOVER_NOTE})"
        ),
        HolderEvidence::ProcessGone {
            pid,
            certain: false,
        } => format!(
            "{opening}, and its process looks gone: no process {pid} is visible from here, so \
             if the holder ran in this PID namespace it has exited. Its lease ({refreshed}) \
             expires in {expires_in}s. Refusing to open a second writer until it does; if the \
             holder has exited, an operator can force a takeover now instead ({TAKEOVER_NOTE})"
        ),
        HolderEvidence::EndpointRefused => format!(
            "{opening}; the lease was {refreshed} and expires in {expires_in}s, but the holder's \
             endpoint is not answering, so it has most likely died. Refusing to open a second \
             writer. If that holder is wedged, an operator can force a takeover \
             ({TAKEOVER_NOTE})"
        ),
    }
}

/// [`HolderEvidence::ProcessGone`], when the holder token names this host and
/// no process with its pid is visible. `None` whenever that cannot be
/// established: another host, a host name that is only the fallback (so it
/// proves nothing about sharing a host), a token in another shape, this very
/// process, or a pid that exists (including one this process may not signal).
fn local_holder_gone(token: &str) -> Option<HolderEvidence> {
    assess_holder(
        token,
        &detect_host(),
        std::process::id(),
        process_instance(),
        process_is_gone,
    )
}

/// [`local_holder_gone`] with this process's facts passed in, so every branch
/// is testable without a second boot or namespace.
fn assess_holder(
    token: &str,
    our_host: &str,
    our_pid: u32,
    ours: &ProcessInstance,
    is_gone: impl Fn(u32) -> bool,
) -> Option<HolderEvidence> {
    let holder = parse_holder(token)?;
    if holder.pid == our_pid || holder.host == UNKNOWN_HOST || holder.host != our_host {
        return None;
    }
    if !is_gone(holder.pid) {
        return None;
    }
    Some(HolderEvidence::ProcessGone {
        pid: holder.pid,
        certain: same_instance(&holder.instance, ours),
    })
}

/// `true` only when the holder provably ran in this boot and this PID
/// namespace (#126). A boot id must be present on both sides and equal. The
/// namespace ids must be present and equal, or both absent on a platform
/// without PID namespaces. A token from before #126 carries neither, so it is
/// never `true`.
fn same_instance(holder: &ProcessInstance, ours: &ProcessInstance) -> bool {
    let same_boot = matches!((&holder.boot_id, &ours.boot_id), (Some(a), Some(b)) if a == b);
    let same_ns = match (holder.pid_ns, ours.pid_ns) {
        (Some(a), Some(b)) => a == b,
        (None, None) => !PLATFORM_HAS_PID_NAMESPACES,
        _ => false,
    };
    same_boot && same_ns
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
            HolderEvidence::ProcessGone {
                pid: 37916,
                certain: false,
            },
        );
        assert_eq!(
            msg,
            "session s is already held by another writer (a@h#37916) — it acquired the \
             single-writer lease 77s ago, and its process looks gone: no process 37916 is \
             visible from here, so if the holder ran in this PID namespace it has exited. Its \
             lease (last refreshed 12s ago) expires in 33s. Refusing to open a second writer \
             until it does; if the holder has exited, an operator can force a takeover now \
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
            HolderEvidence::ProcessGone {
                pid: 9,
                certain: false,
            },
            HolderEvidence::ProcessGone {
                pid: 9,
                certain: true,
            },
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
    fn timing_is_read_from_one_instant() {
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

        // #117 review L1: skew can put the row's expiry further out than a
        // fresh lease allows only by under a TTL, and then the arithmetic
        // "refreshed before it was taken" is clamped to the lease's age.
        let young = LeaseInfo {
            expires_at: acquired_at + chrono::Duration::seconds(5) + chrono::Duration::seconds(30),
            ..long
        };
        let t = LeaseTiming::from_row(&young, Duration::from_secs(5));
        assert_eq!(t.held_for, 5);
        assert_eq!(t.expires_in, 30);
        assert_eq!(
            t.refreshed_ago,
            Some(5),
            "clamped to the lease's age, not 15"
        );
    }

    /// #126: only a holder in this boot and PID namespace is called gone
    /// without a hedge.
    #[test]
    fn a_gone_holder_in_this_boot_and_namespace_is_named_plainly() {
        let msg = format_lease_held(
            &session(),
            "a@h#37916;boot=b1;pidns=7",
            timing(),
            HolderEvidence::ProcessGone {
                pid: 37916,
                certain: true,
            },
        );
        assert_eq!(
            msg,
            "session s is already held by another writer (a@h#37916;boot=b1;pidns=7) — it \
             acquired the single-writer lease 77s ago, and the holder process is gone: it ran \
             in this boot and PID namespace, and no process 37916 exists. Its lease (last \
             refreshed 12s ago) expires in 33s. Refusing to open a second writer until it \
             does; an operator can force a takeover now instead (see the single-writer lease \
             note in docs/reference/cli.mdx)"
        );
        assert!(!msg.contains("looks gone"), "{msg}");
    }

    fn instance(boot: Option<&str>, ns: Option<u64>) -> ProcessInstance {
        ProcessInstance {
            boot_id: boot.map(str::to_string),
            pid_ns: ns,
        }
    }

    /// Every branch of the #126 decision, with this process's facts
    /// injected: which tokens get the plain wording, which keep the hedge,
    /// and which get no hint at all.
    #[test]
    fn every_hint_branch_of_the_holder_assessment() {
        let gone = |_: u32| true;
        let alive = |_: u32| false;
        let ours = instance(Some("b1"), Some(7));
        let assess = |token: &str, ours: &ProcessInstance, is_gone: &dyn Fn(u32) -> bool| {
            assess_holder(token, "h", 100, ours, is_gone)
        };
        let hedged = Some(HolderEvidence::ProcessGone {
            pid: 5,
            certain: false,
        });
        let plain = Some(HolderEvidence::ProcessGone {
            pid: 5,
            certain: true,
        });

        // Same boot, same namespace, pid not visible: plain.
        assert_eq!(assess("a@h#5;boot=b1;pidns=7", &ours, &gone), plain);
        // An older writer's token: hedged, as in 0.5.0.
        assert_eq!(assess("a@h#5", &ours, &gone), hedged);
        // Another boot (a reboot reused the pid), or another namespace (a
        // container on the host network): hedged.
        assert_eq!(assess("a@h#5;boot=b2;pidns=7", &ours, &gone), hedged);
        assert_eq!(assess("a@h#5;boot=b1;pidns=8", &ours, &gone), hedged);
        // Only one of the two ids: hedged.
        assert_eq!(assess("a@h#5;boot=b1", &ours, &gone), hedged);
        assert_eq!(assess("a@h#5;pidns=7", &ours, &gone), hedged);
        // This process cannot read its own boot id: hedged.
        assert_eq!(
            assess("a@h#5;boot=b1;pidns=7", &instance(None, Some(7)), &gone),
            hedged
        );
        // Both sides without a namespace id: plain only where there are no
        // PID namespaces.
        let no_ns = instance(Some("b1"), None);
        let expect = if PLATFORM_HAS_PID_NAMESPACES {
            hedged
        } else {
            plain
        };
        assert_eq!(assess("a@h#5;boot=b1", &no_ns, &gone), expect);
        // No hint at all: the pid is visible, another host, the fallback
        // host, this very process, a token in another shape.
        assert_eq!(assess("a@h#5;boot=b1;pidns=7", &ours, &alive), None);
        assert_eq!(assess("a@other#5;boot=b1;pidns=7", &ours, &gone), None);
        assert_eq!(
            assess_holder(
                &format!("a@{UNKNOWN_HOST}#5;boot=b1;pidns=7"),
                UNKNOWN_HOST,
                100,
                &ours,
                gone
            ),
            None
        );
        assert_eq!(assess("a@h#100;boot=b1;pidns=7", &ours, &gone), None);
        assert_eq!(assess("lambo:released", &ours, &gone), None);
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
            // A pre-#126 token: the hedge.
            assert_eq!(
                local_holder_gone(&format!("a@{host}#{pid}")),
                Some(HolderEvidence::ProcessGone {
                    pid,
                    certain: false
                })
            );
            // Our own boot and namespace: plain, where the OS reports them.
            let ours = process_instance();
            let token = format!(
                "a@{host}#{pid}{}{}",
                ours.boot_id
                    .as_deref()
                    .map(|b| format!(";boot={b}"))
                    .unwrap_or_default(),
                ours.pid_ns
                    .map(|n| format!(";pidns={n}"))
                    .unwrap_or_default()
            );
            let certain =
                ours.boot_id.is_some() && (ours.pid_ns.is_some() || !PLATFORM_HAS_PID_NAMESPACES);
            assert_eq!(
                local_holder_gone(&token),
                Some(HolderEvidence::ProcessGone { pid, certain })
            );
            // Same pid on another host: not ours to judge.
            assert_eq!(
                local_holder_gone(&format!("a@{host}-elsewhere#{pid}")),
                None
            );
        }
        // pid 1 exists on every unix (and is not ours to signal): alive.
        assert!(!process_is_gone(1));
    }

    /// #117 review M2: two processes that both fell back to the placeholder
    /// host name share the string, not a host, so the check never fires for
    /// it — whatever the pid.
    #[test]
    fn the_fallback_host_name_never_counts_as_this_host() {
        assert_eq!(
            local_holder_gone(&format!("a@{UNKNOWN_HOST}#{}", u32::MAX - 1)),
            None
        );
    }
}
