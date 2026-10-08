//! `lambo erase-session` — erase one session for account deletion (#23).
//!
//! An operator verb, not an agent one: it is not an MCP tool, and the
//! authority to run it is access to the store itself (the DSN or the SQLite
//! file), never an `agent_id`. `--confirm` must repeat `--session` exactly, so
//! a typo or a shell-history slip erases nothing.
//!
//! It never preempts a live writer. While a `lambo serve` (or any writer
//! verb) holds the session's lease, the erase is refused and names the holder;
//! stop that writer (its close flushes and releases the lease) and run it
//! again. A writer whose lease already lapsed is fenced by the erase and
//! winds down at its next heartbeat.
//!
//! Output is one JSON object on stdout, the store's [`EraseReport`]: counts
//! removed per kind, `already_absent` (nothing was left to remove, as on a
//! repeat), and the tombstone's fencing token. It is printed only after the
//! store committed, so a deletion fan-out may mark the target done on exit 0.
//!
//! [`EraseReport`]: crate::store::EraseReport

use super::caps::{check_size_cli, require_nonempty, CliError};
use crate::store::lease::LeaseHolder;
use crate::store::{EraseOutcome, GraphStore};
use crate::types::{AgentId, SessionId};

/// The agent id the erasing process's lease holder is stamped with.
pub const ERASER_AGENT: &str = "lambo-erase-session";

/// Parsed `lambo erase-session` flags.
#[derive(Debug)]
pub struct Args {
    pub session: String,
    pub confirm: String,
}

/// Erase `args.session` from `store`, returning the report as JSON.
pub async fn run(store: &dyn GraphStore, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    if args.confirm != args.session {
        return Err(CliError::Usage(format!(
            "--confirm must repeat --session exactly (got --session '{}' and --confirm '{}'); \
             nothing was erased",
            args.session, args.confirm
        )));
    }
    let session = SessionId::new(args.session);
    let eraser = LeaseHolder::for_this_process(&AgentId::new(ERASER_AGENT));
    match store.erase_session(&session, &eraser).await {
        Ok(EraseOutcome::Erased(report)) => serde_json::to_string(&report)
            .map_err(|e| CliError::Runtime(format!("render the erase report: {e}"))),
        Ok(EraseOutcome::Held { current, age }) => Err(CliError::Runtime(format!(
            "session {session} is held by a live writer ({}, holding the single-writer lease \
             for {}s); nothing was erased. Stop that writer (a clean stop flushes and releases \
             the lease), then run erase-session again",
            current.holder,
            age.as_secs(),
        ))),
        Err(e) => Err(CliError::Runtime(format!("erase_session: {e}"))),
    }
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::store::{LeaseOutcome, MemoryStore};

    fn args(session: &str, confirm: &str) -> Args {
        Args {
            session: session.into(),
            confirm: confirm.into(),
        }
    }

    #[tokio::test]
    async fn a_confirm_that_does_not_repeat_the_session_is_a_usage_error() {
        let store = MemoryStore::new();
        let err = run(&store, args("user-42", "user-24")).await.unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        assert!(err.to_string().contains("nothing was erased"), "{err}");
        assert!(
            store
                .read_lease(&SessionId::new("user-42"))
                .await
                .unwrap()
                .is_none(),
            "a refused confirm writes no tombstone"
        );
    }

    #[tokio::test]
    async fn an_empty_session_is_a_usage_error() {
        let err = run(&MemoryStore::new(), args("", "")).await.unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
    }

    #[tokio::test]
    async fn the_report_is_one_json_object_and_a_repeat_is_already_absent() {
        let store = MemoryStore::new();
        let out = run(&store, args("user-42", "user-42")).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["session"], "user-42");
        assert_eq!(v["already_absent"], true);
        assert_eq!(v["fence_token"], 1);
        for kind in [
            "sessions",
            "interactions",
            "concepts",
            "vectors",
            "edges",
            "synonyms",
            "canonization_events",
            "reservations",
            "write_intents",
            "session_stats",
            "lease_refusals",
            "leases",
        ] {
            assert_eq!(v["removed"][kind], 0, "{kind}");
        }
    }

    #[tokio::test]
    async fn a_live_writer_refuses_the_erase_and_is_named() {
        let store = MemoryStore::new();
        let writer = LeaseHolder::for_this_process(&AgentId::new("serve-agent"));
        let outcome = store
            .acquire_lease(&SessionId::new("user-42"), &writer, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(matches!(outcome, LeaseOutcome::Acquired(_)));
        let err = run(&store, args("user-42", "user-42")).await.unwrap_err();
        assert_eq!(err.exit_code(), 1, "{err}");
        let msg = err.to_string();
        assert!(msg.contains(&writer.token()), "{msg}");
        assert!(msg.contains("nothing was erased"), "{msg}");
    }
}
