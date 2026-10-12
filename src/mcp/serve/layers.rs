//! A layered serve's start checks (#33 PR 3, design section 1).
//!
//! `[serve]`'s own validation (names, the one-layer limit, no self or
//! transitive layering) and `ServeConfig::resolve_layers` (every layer and
//! layered session pinned) run before this. What only a serve can check is
//! here, each a hard error that refuses the start:
//!
//! * [`check_pinned_layers`]: the stacks a library caller hands
//!   `ServeOptions` name only sessions this serve pins (the CLI's come from
//!   `resolve_layers` over the same pinned set);
//! * [`check_coverage`], before any lease: every credential reaching a
//!   layered session reaches each of its layers, so no runtime "the layer
//!   exists but you cannot see it" answer exists (no enumeration oracle);
//! * [`check_contracts`], once the pinned sessions are acquired: a session
//!   and its layers share one embedding contract, so one query vector is
//!   valid everywhere and cosines are comparable.
//!
//! A stdio holder holds one session and serves it unlayered, with one
//! warning ([`stdio_unlayered_warning`]); a stdio serve that proxies to an
//! HTTP holder gets that holder's layering and warns of nothing.

use super::authority::ServeAuthority;
use super::Transport;
use crate::config::ServeLayers;
use crate::surface::session::SessionNeed;
use crate::types::{EmbeddingContract, LamboError};

fn layer_err(msg: impl std::fmt::Display) -> LamboError {
    LamboError::Config(format!("lambo serve layers: {msg}"))
}

/// Every layered session and every layer in `layers` is one of `pinned`.
pub(super) fn check_pinned_layers(
    layers: &ServeLayers,
    pinned: &[String],
) -> Result<(), LamboError> {
    let is_pinned = |s: &str| pinned.iter().any(|p| p == s);
    for (session, stack) in layers.iter() {
        if !is_pinned(session) {
            return Err(layer_err(format!(
                "{session:?} is layered but not pinned: only pinned sessions are layered"
            )));
        }
        for layer in stack {
            if !is_pinned(&layer.session) {
                return Err(layer_err(format!(
                    "{session:?} is layered with {:?}, which this serve does not pin: every \
                     layer must be pinned",
                    layer.session
                )));
            }
        }
    }
    Ok(())
}

/// Every credential that may use a layered session may use each of its
/// layers. Names the credential and both sessions, never a token. The
/// implicit `local` and legacy `default` grants cover every pinned session,
/// so a serve using only those always passes.
pub(super) fn check_coverage(
    authority: &ServeAuthority,
    layers: &ServeLayers,
) -> Result<(), LamboError> {
    for grant in authority.grants() {
        for (session, stack) in layers.iter() {
            if authority
                .authorize(grant, session, SessionNeed::Use)
                .is_err()
            {
                continue;
            }
            for layer in stack {
                if authority
                    .authorize(grant, &layer.session, SessionNeed::Use)
                    .is_err()
                {
                    return Err(layer_err(format!(
                        "credential {:?} reaches {session:?} but not its layer {:?}: a layered \
                         recall would read a session the credential cannot, so add {:?} to \
                         the credential's sessions (or take the layer off {session:?})",
                        grant.name(),
                        layer.session,
                        layer.session
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Each layered session and each of its layers that `contract_of` knows
/// (the pinned sessions this start acquired) have one embedding contract.
///
/// The library compares contracts exactly, so a `model` unset in one and
/// empty in the other differ, and the layer would be unavailable on every
/// recall. That case is refused with its own message (the #134 note's L6)
/// rather than normalised here, where a normalised start would still leave
/// every call degraded.
pub(super) fn check_contracts<'a>(
    layers: &ServeLayers,
    contract_of: impl Fn(&str) -> Option<&'a EmbeddingContract>,
) -> Result<(), LamboError> {
    for (session, stack) in layers.iter() {
        let Some(own) = contract_of(session) else {
            continue;
        };
        for layer in stack {
            let Some(theirs) = contract_of(&layer.session) else {
                continue;
            };
            if own == theirs {
                continue;
            }
            let model_blank_only = own.kind == theirs.kind
                && own.dim == theirs.dim
                && own.model.as_deref().unwrap_or("") == theirs.model.as_deref().unwrap_or("");
            return Err(layer_err(if model_blank_only {
                format!(
                    "{session:?} and its layer {:?} differ only in the embedding contract's \
                     model: unset in one, an empty string in the other. Contracts are \
                     compared exactly; set the model the same way for both",
                    layer.session
                )
            } else {
                format!(
                    "{session:?} and its layer {:?} have different embedding contracts \
                     ({own:?} and {theirs:?}): a layer must share its consumer's embedding \
                     space",
                    layer.session
                )
            }));
        }
    }
    Ok(())
}

/// The one warning a stdio holder logs when its session would be layered
/// over HTTP (design section 1, Stdio): it holds one session and serves it
/// unlayered. `serve` logs it only on the holder path, after the
/// election: a stdio serve that proxies to an HTTP holder reaches that
/// holder's session, layers included, and must not claim otherwise.
/// `None` for an unlayered session.
pub(super) fn stdio_unlayered_warning(session: &str, layers: &ServeLayers) -> Option<String> {
    let stack = layers.stack(session);
    (!stack.is_empty()).then(|| {
        let names: Vec<&str> = stack.iter().map(|l| l.session.as_str()).collect();
        format!(
            "lambo.toml [serve] layers {session:?} with {}, but this stdio serve holds the \
             session itself and serves it unlayered; recall through the HTTP serve \
             (/mcp/s/{session}) to read its layers",
            names.join(", ")
        )
    })
}

/// The layer stacks a serve over `transport` pinning `sessions` uses:
/// `[serve]`'s, resolved, over HTTP. Over stdio, the one session's stack
/// by configuration alone, unchecked: `serve` never layers a stdio
/// session, and uses it only to warn if it ends up the holder.
pub fn plan_layers(
    cfg: &crate::config::ServeConfig,
    sessions: &[String],
    transport: Transport,
) -> Result<ServeLayers, LamboError> {
    match transport {
        Transport::Stdio => Ok(sessions
            .first()
            .map(|s| cfg.stdio_layers(s))
            .unwrap_or_default()),
        Transport::Http => cfg.resolve_layers(sessions),
    }
}

/// One startup warning per layer that is erased once the pinned sessions
/// are in (#33 PR 3 review L1): every layered recall of its consumers will
/// answer without it for the life of the serve, which an operator should
/// hear about at start, not from an agent. `erased` says whether a layer
/// is erased (the registry's slot).
pub(super) fn erased_layer_warnings(
    layers: &ServeLayers,
    erased: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for (session, stack) in layers.iter() {
        for layer in stack {
            if erased(&layer.session) {
                out.push(format!(
                    "layer {:?} of {session:?} is erased: every layered recall of {session:?} \
                     answers without it until the layer is taken off [serve] layers",
                    layer.session
                ));
            }
        }
    }
    out
}
