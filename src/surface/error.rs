//! Model-safe classification of a [`LamboError`] (N4), shared by every path
//! that tells a caller a write or a tool call failed.
//!
//! The full error can carry a DSN, a store URL, a file path or a driver
//! message; a caller (often a model) gets only the class, and the detail goes
//! to the operator's log at the call site.

use crate::types::LamboError;

/// A short, detail-free class for a `Memory` failure (N4).
///
/// The full error can interpolate a DSN, a store URL, a file path or a driver
/// message — none of which the model needs and any of which is worth keeping
/// out of a model-facing string. Return the class; the detail is logged.
///
/// Shared since JE2E-12, so `writeq`'s async write path renders the same class
/// MCP's synchronous path (`mcp::server`'s `tool_err`) does. The alternative was
/// a second match in `writeq.rs`, which is how a new `LamboError` variant would
/// come to have one class on the sync path and another on the async one — the
/// drift this function exists to prevent, reintroduced one module over. It
/// lives here rather than in `mcp::server` (#25) so the core write queue does
/// not import the MCP surface for it. The old `crate::mcp::server::err_class`
/// path was crate-private and is gone since #25 split `mcp::server`: nothing
/// used it once every caller imported this module directly.
pub(crate) fn err_class(err: &LamboError) -> &'static str {
    match err {
        LamboError::Store(_) => "store error",
        // J3 round-1 N1: same pairing as the Conflict/SoftLock one below. The
        // split lets the durable-intent replay tell "not reached" from "refused
        // this input"; it must not give the model, the operator or the ledger a
        // new error class to learn.
        LamboError::Embed(_) | LamboError::EmbedUnavailable(_) => "embedding error",
        LamboError::Config(_) => "configuration error",
        // J1-R2-2: two variants, one class. The split exists so N4 can tell a
        // model-safe §11 refusal from a lease-lost one; an operator and the
        // ledger see the same `error_kind` either way, so nothing downstream of
        // this function moved.
        LamboError::Conflict(_) => "conflict",
        LamboError::SoftLock(_) => "conflict",
        // #22 PR 4: its own class, so the ledger's `error_kind` tells a
        // caller-fixable id collision from an embedder refusal.
        LamboError::ImageIdTaken(_) => "image id taken",
        LamboError::Other(_) => "internal error",
    }
}

/// What a caller (often a model) is told about a failed write or tool call:
/// the class from [`err_class`] and a pointer to the log, the same sentence on
/// the synchronous path (`mcp::server`'s `tool_err`) and on a write receipt
/// (`writeq`'s `model_safe_failure`).
///
/// **One exception, by type** (the J1-R2-2 rule): a
/// [`LamboError::ImageIdTaken`] says what happened and how to fix it, because
/// its only field is the caller's own image id. Even that id is shown only
/// when it has the published shape (`[a-z0-9]{1,64}`), so nothing else can
/// ride on it.
///
/// **And one by kind** (#22 PR 6 review Low 1): a store error that is a
/// vector read refusing a probe outside the session's embedding space
/// ([`crate::types::StoreError::is_embedding_contract_refusal`]: the contract
/// changed mid-query, or the width differs) says to re-check the contract and
/// retry. The text is fixed; nothing from the refusal is echoed, and the
/// class (`err_class`, the ledger's `error_kind`) is still `store error`.
pub(crate) fn model_safe_message(err: &LamboError) -> String {
    match err {
        LamboError::ImageIdTaken(id) => {
            let named = if crate::graph::image::validate_image_id(id).is_ok() {
                format!(" {id:?}")
            } else {
                String::new()
            };
            format!(
                "image id taken: a text concept in this session already holds this caption with \
                 image id{named}, so the image would have no vector of its own; choose another \
                 image id"
            )
        }
        LamboError::Store(e) if e.is_embedding_contract_refusal() => {
            "store error: the session's embedding contract no longer matches this query \
             (it changed mid-query, or the vector width differs); re-check the session's \
             embedding contract (lambo_stats) and retry"
                .into()
        }
        other => format!("{} (the detail was logged server-side)", err_class(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #22 PR 4: the image-id collision names the fix and the caller's own
    /// id, and nothing else; every other error is still a bare class.
    #[test]
    fn an_image_id_collision_names_the_fix_and_only_the_callers_id() {
        let msg = model_safe_message(&LamboError::ImageIdTaken("r17".into()));
        assert!(msg.contains("choose another image id"), "{msg}");
        assert!(msg.contains("\"r17\""), "{msg}");
        assert_eq!(
            err_class(&LamboError::ImageIdTaken("r17".into())),
            "image id taken"
        );

        // An id that is not the published shape is not echoed at all.
        let odd = model_safe_message(&LamboError::ImageIdTaken("/srv/x\nline".into()));
        assert!(!odd.contains("/srv") && !odd.contains('\n'), "{odd}");
        assert!(odd.contains("choose another image id"), "{odd}");

        let other = model_safe_message(&LamboError::Embed("/srv/secret.sqlite".into()));
        assert_eq!(other, "embedding error (the detail was logged server-side)");
    }

    /// #22 PR 6 review Low 1: a contract race or a width mismatch on a
    /// vector read says to re-check the contract and retry, distinct from a
    /// generic store error, and echoes nothing of the refusal (the session
    /// id, the models, the widths). Any other invariant stays a bare class.
    #[test]
    fn a_contract_refusal_says_to_recheck_and_retry_and_echoes_nothing() {
        use crate::types::StoreError;
        for refusal in [
            "vector candidate lookup refused after embedding contract changed: \
             stored kind=bge model=\"/srv/models/m.gguf\" dim=1024",
            "query embedding has 768 dimensions but session secret-sess stores vectors of 1024",
        ] {
            let err = LamboError::Store(StoreError::Invariant(refusal.into()));
            let msg = model_safe_message(&err);
            assert!(msg.starts_with("store error: "), "{msg}");
            assert!(msg.contains("re-check") && msg.contains("retry"), "{msg}");
            for leak in ["/srv", "secret-sess", "768", "1024", "bge"] {
                assert!(!msg.contains(leak), "{leak} in {msg}");
            }
            assert_eq!(err_class(&err), "store error");
        }
        for other in [
            StoreError::Invariant("concept x carries a vector of 3 dimensions".into()),
            StoreError::Backend(
                "vector candidate lookup refused after embedding contract changed".into(),
            ),
        ] {
            assert_eq!(
                model_safe_message(&LamboError::Store(other)),
                "store error (the detail was logged server-side)"
            );
        }
    }
}
