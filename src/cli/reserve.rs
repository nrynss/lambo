//! `lambo reserve` / `lambo release` — lease-held thin adapters over
//! [`crate::memory::Memory::reserve`] / [`crate::memory::Memory::release`].
//!
//! Reservations are advisory and RAM-local (pinned contract S5). On the CLI
//! they end when this process exits (`close_writer` is the next line), not
//! at a printed `until` instant and not "on restart".

use std::time::Duration;

use uuid::Uuid;

use super::caps::{check_size_cli, require_nonempty, CliError, MAX_RESERVE_TTL_SECS};
use super::{close_writer, open_writer};
use crate::resolve::ResolvedBackends;
use crate::types::NodeId;

/// Parsed `reserve` flags.
pub struct ReserveArgs {
    pub session: String,
    pub agent: String,
    pub node: String,
    pub ttl_seconds: Option<u64>,
}

/// Parsed `release` flags.
pub struct ReleaseArgs {
    pub session: String,
    pub agent: String,
    pub node: String,
}

fn parse_node(raw: &str) -> Result<NodeId, CliError> {
    require_nonempty("node", raw)?;
    check_size_cli("node", raw)?;
    Uuid::parse_str(raw.trim())
        .map(NodeId)
        .map_err(|e| CliError::Usage(format!("node must be a UUID: {e}")))
}

/// Take a soft lock on a memory node.
pub async fn reserve(backends: ResolvedBackends, args: ReserveArgs) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    let node_id = parse_node(&args.node)?;

    let ttl_secs = args.ttl_seconds.unwrap_or(30);
    if ttl_secs == 0 || ttl_secs > MAX_RESERVE_TTL_SECS {
        return Err(CliError::Usage(format!(
            "ttl-seconds must be in 1..={MAX_RESERVE_TTL_SECS}"
        )));
    }

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let out = match mem.reserve(node_id, Duration::from_secs(ttl_secs)) {
        Ok(reservation) => {
            let summary = format!(
                "reserved {} for agent '{}'\n\
                 reservations are advisory and RAM-local: this reservation ends when \
                 this process exits (now). The TTL that would apply inside a long-lived \
                 writer such as serve is {}s (expires_at {} is not a CLI hold)",
                node_id.0,
                reservation.agent_id.0,
                ttl_secs,
                reservation.expires_at.to_rfc3339(),
            );
            Ok(summary)
        }
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}

/// Release this agent's soft lock on a node.
pub async fn release(backends: ResolvedBackends, args: ReleaseArgs) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;
    require_nonempty("agent", &args.agent)?;
    check_size_cli("agent", &args.agent)?;
    let node_id = parse_node(&args.node)?;

    let mem = open_writer(backends, &args.session, &args.agent).await?;
    let out = match mem.release(node_id) {
        Ok(()) => Ok(format!("released {}", node_id.0)),
        Err(e) => Err(CliError::from(e)),
    };
    close_writer(mem, out).await
}
