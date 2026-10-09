//! `lambo recall-index backfill` — rebuild a session's recall index (#18).
//!
//! An operator verb for the recall tier (`[recall]` in `lambo.toml`). It
//! takes the session's single-writer lease for the duration, so it is
//! refused while a writer holds the session: a holder repairs its own index
//! when it loads the session and after a failed mirror, and this verb is for
//! everything else (a repair after an outage with no writer running, or a
//! fresh index for a session that was written before the tier existed).
//!
//! It re-indexes every stored concept vector from the durable store, drops
//! index documents the durable state no longer has (deleted concepts, an
//! older embedding contract's index), and prints one JSON report.

use super::caps::{check_size_cli, require_nonempty, CliError};
use crate::store::lease::LeaseHolder;
use crate::store::GraphStore;
use crate::types::{AgentId, SessionId};

/// The agent id the backfill's lease holder is stamped with.
pub const BACKFILL_AGENT: &str = "lambo-recall-index-backfill";

/// Parsed `lambo recall-index backfill` flags.
#[derive(Debug)]
pub struct Args {
    pub session: String,
}

/// Rebuild `args.session`'s recall index, returning the report as JSON.
pub async fn backfill(store: &dyn GraphStore, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    let session = SessionId::new(args.session);
    let holder = LeaseHolder::for_this_process(&AgentId::new(BACKFILL_AGENT));
    match store.backfill_recall_index(&session, &holder).await {
        Ok(Some(report)) => serde_json::to_string(&report)
            .map_err(|e| CliError::Runtime(format!("render the backfill report: {e}"))),
        Ok(None) => Err(CliError::Usage(
            "this configuration has no recall tier: add a [recall] section to lambo.toml \
             (see lambo.example.toml); nothing was rebuilt"
                .into(),
        )),
        Err(e) => Err(CliError::Runtime(format!("recall-index backfill: {e}"))),
    }
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    #[tokio::test]
    async fn a_store_without_a_tier_is_a_usage_error() {
        let err = backfill(
            &MemoryStore::new(),
            Args {
                session: "s".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        assert!(err.to_string().contains("[recall]"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_session_is_a_usage_error() {
        let err = backfill(
            &MemoryStore::new(),
            Args {
                session: String::new(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
    }
}
