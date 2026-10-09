//! Which sessions a `lambo serve` pins (#32 PR 4, design §2.5 and §7.2):
//! the ordered union of the repeatable `--session` and `[serve] sessions`,
//! and the default session `/mcp` serves.
//!
//! PR 4 serves pinned sessions only: every session a serve hosts is attached
//! at startup and stays attached. On-demand sessions (PR 6) and credentials
//! (PR 5) build on this. The stdio cwd map (PR 8) chooses a stdio serve's one
//! session before this runs (`ServeConfig::select_stdio_session`).

use super::Transport;
use crate::config::ServeConfig;
use crate::surface::session::{parse_addressed, MAX_ADDRESSED_LEN};
use crate::types::LamboError;

/// The sessions a serve pins, and the default among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedSessions {
    /// What `/mcp` serves, and what a stdio serve owns: the first
    /// `--session`, else `[serve] default_session`, else the first pinned.
    pub default: String,
    /// Every pinned session, in order: the `--session` values first, then
    /// `[serve] sessions`, each once.
    pub sessions: Vec<String>,
}

/// Work out the pinned sessions from the command line and `[serve]`.
///
/// * **stdio** owns exactly one session (design §2.2, Q9), and
///   `[serve] sessions` does not apply. The CLI chooses it first
///   (`--session`, else PR 8's cwd map, else `default_session`) and passes
///   it here as the one value; a library caller that passes none is refused.
/// * **HTTP** pins the ordered union of `--session` and `[serve] sessions`,
///   and needs at least one. The default must be pinned (PR 4 hosts nothing
///   else), and the union must fit `max_attached`, which `[serve]`'s own
///   validation could only check for its own list (PR 1 review).
///
/// The result then meets `serve`'s own rules (`check_pinned`: with more
/// than one session, every name is addressable by URL), so the CLI refuses
/// a bad name here, before any backend or model is built, rather than in
/// `serve` after them (#32 review L3). `serve` keeps its own call for
/// library callers that build `ServeOptions` by hand.
pub fn pin_sessions(
    cli: &[String],
    cfg: &ServeConfig,
    transport: Transport,
) -> Result<PinnedSessions, LamboError> {
    let pinned = plan_sessions(cli, cfg, transport)?;
    check_pinned(&pinned.default, &pinned.sessions, transport)?;
    Ok(pinned)
}

/// [`pin_sessions`] before `serve`'s rules are applied.
fn plan_sessions(
    cli: &[String],
    cfg: &ServeConfig,
    transport: Transport,
) -> Result<PinnedSessions, LamboError> {
    match transport {
        Transport::Stdio => match cli {
            [one] => Ok(PinnedSessions {
                default: one.clone(),
                sessions: vec![one.clone()],
            }),
            [] => Err(LamboError::Config(
                "--session <SESSION> is required: a stdio serve owns exactly one session".into(),
            )),
            _ => Err(LamboError::Config(format!(
                "--session was given {} times, but a stdio serve owns exactly one session; \
                 serve several sessions with --transport http",
                cli.len()
            ))),
        },
        Transport::Http => {
            let mut sessions: Vec<String> = Vec::new();
            for name in cli.iter().chain(cfg.sessions.iter()) {
                if !sessions.contains(name) {
                    sessions.push(name.clone());
                }
            }
            let Some(first) = sessions.first().cloned() else {
                return Err(LamboError::Config(
                    "--session <SESSION> is required, or name the sessions to serve in \
                     lambo.toml [serve] sessions"
                        .into(),
                ));
            };
            let default = cli
                .first()
                .cloned()
                .or_else(|| cfg.default_session.clone())
                .unwrap_or(first);
            if !sessions.contains(&default) {
                return Err(LamboError::Config(format!(
                    "lambo.toml [serve] default_session {default:?} is not one of the sessions \
                     this serve pins: list it in [serve] sessions (this release serves pinned \
                     sessions only)"
                )));
            }
            if sessions.len() > cfg.max_attached() {
                return Err(LamboError::Config(format!(
                    "{} pinned sessions (--session and [serve] sessions together) exceed \
                     max_attached ({}): every pinned session stays attached, so raise \
                     [serve] max_attached or pin fewer sessions",
                    sessions.len(),
                    cfg.max_attached()
                )));
            }
            Ok(PinnedSessions { default, sessions })
        }
    }
}

/// `serve`'s own check of the sessions in its options, so a library caller
/// that builds `ServeOptions` by hand meets the same rules as the CLI:
/// `session` is pinned, nothing is pinned twice, stdio pins one, and with
/// more than one every name can be addressed by URL (`/mcp/s/{session}`,
/// decision 16). A single session keeps `--session`'s looser rule and is
/// served at `/mcp`, so nothing deployed breaks.
pub(super) fn check_pinned(
    session: &str,
    sessions: &[String],
    transport: Transport,
) -> Result<(), LamboError> {
    if !sessions.iter().any(|s| s == session) {
        return Err(LamboError::Config(format!(
            "ServeOptions: the default session {session:?} is not in `sessions`"
        )));
    }
    for (i, name) in sessions.iter().enumerate() {
        if sessions[..i].contains(name) {
            return Err(LamboError::Config(format!(
                "ServeOptions: session {name:?} is pinned twice"
            )));
        }
    }
    if sessions.len() > 1 {
        if transport == Transport::Stdio {
            return Err(LamboError::Config(
                "a stdio serve owns exactly one session; serve several with --transport http"
                    .into(),
            ));
        }
        for name in sessions {
            if parse_addressed(name).is_err() {
                return Err(LamboError::Config(format!(
                    "session {name:?} cannot be addressed by URL: with more than one session, \
                     each is served at /mcp/s/<session> and must be 1 to {MAX_ADDRESSED_LEN} \
                     bytes of [A-Za-z0-9._:-], not starting with '.'. A session outside that \
                     rule can still be served on its own with `lambo serve --session <name>`"
                )));
            }
        }
    }
    Ok(())
}
