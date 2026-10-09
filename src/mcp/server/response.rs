//! What a tool call hands back when it does not simply succeed: model-safe
//! errors (N4), the second text block for warnings (R1/T82-9), piggybacked
//! write receipts (J3), and panic containment (R1/T82-5).

use std::future::Future;

use rmcp::model::{CallToolResult, ContentBlock};

use super::params::breaks_one_line;
use super::trace::note_error;
use crate::store::flush::{panic_message, CatchUnwindPoll};
use crate::surface::error::{err_class, model_safe_message};
use crate::types::LamboError;
use crate::writeq::{ReceiptAnswer, ReceiptId};

/// Render a `Memory` failure as a caller-visible tool error (N4).
///
/// Matches the [`contain_panic`] policy: the full detail goes to the log, the
/// client gets a class and a pointer to the log — never the raw error, which
/// can carry a store URL or driver message.
///
/// **One documented exception**, added by J1-R1-2 and narrowed by J1-R2-2:
/// [`conflict_err`] renders a [`LamboError::SoftLock`] with its message intact
/// on the reserve path, because that message is a node id, a holder and an
/// expiry — nothing N4 exists to hide, and the only way a caller can learn who
/// to wait for. Every other error on every path comes through here, including
/// every [`LamboError::Conflict`]: the lease-lost fence is one, and its message
/// is exactly the operator-only detail N4 exists for.
pub(super) fn tool_err(what: &str, err: LamboError) -> CallToolResult {
    tracing::error!(
        tool = what,
        error = %err,
        "mcp: tool returned a Memory error — full detail logged, class returned to the caller"
    );
    // I1: the same class the caller is told, in the ledger's `error_kind`.
    note_error(err_class(&err));
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{what}: {}",
        model_safe_message(&err)
    ))])
}

/// Render a §11 soft-lock refusal from the reserve path as a model-facing
/// refusal that still carries its detail (J1-R1-2).
///
/// [`tool_err`]'s N4 policy discards a `Memory` error's message because it can
/// interpolate a DSN, a store URL, a file path or a driver string — none of
/// which the model needs. `graph::reserve`'s two messages carry none of that:
/// they are built from a node id the caller just sent, the holder's `agent_id`,
/// and an expiry — and the last two are *already* model-facing, since `recall`
/// renders that same holder and expiry into the context block. They are also
/// precisely what the loser of a race needs, because the whole
/// cooperative-identity design is "coordinate by ids"; a bare `conflict` leaves
/// a caller unable to tell a lock it should wait for from one it should work
/// around.
///
/// **What selects this function is the producer, not the class (J1-R2-2).** The
/// first version of this exception matched [`LamboError::Conflict`], and that
/// was wrong: `Memory::reserve_as`/`release_as` enter `begin_write_sync()`
/// *before* the graph, and a fenced handle's `lease_lost_error` was a `Conflict`
/// too — one interpolating `store::lease::OPERATOR_OVERRIDE`, a raw
/// `DELETE FROM session_leases …`. So `lambo_reserve` handed a model an
/// operator-only statement against an internal table, on a path where the
/// parent returned a class. Matching a *variant* opens the door for every
/// producer of that variant, not for the one this docstring reasons about, and
/// no amount of care in this function could have narrowed it — which is why
/// `graph::reserve` now returns its own [`LamboError::SoftLock`] and this
/// exception is spelled against that. The default is closed: a new `Conflict`
/// producer anywhere under `reserve_as` flattens through [`tool_err`] without
/// anyone having to remember this paragraph. `redact_urls` was never the
/// missing piece — the leaked string had no `://`.
///
/// The exception stays as narrow as it reads: everything else on this path
/// still goes through [`tool_err`], and the ledger books the same `error_kind`
/// (`"conflict"`) either way, so the split is invisible downstream of
/// [`err_class`]. The appended "wait for the expiry or work elsewhere" advice is
/// true again for the same reason: a §11 soft lock does expire, whereas the
/// fenced handle this function used to reach never will.
///
/// The message is folded to one line on the way out, by the same
/// [`breaks_one_line`] class [`super::LamboServer::check_agent_id`] refuses at the door
/// — one predicate, so the guard and the fold cannot disagree about what "one
/// line" means (they did: J1-R2-1). The fold is defence in depth, for a holder
/// that entered by another path — a library caller, or an operator's `--agent`.
///
/// It does **not** [`redact_urls`]. N3's redaction exists for Lambo's own
/// endpoints appearing in Lambo's own warnings; the holder here is a
/// caller-chosen name, which `recall` already renders verbatim into the context
/// block through `format::reservation_warning`. Redacting this one path would
/// advertise a neutralisation the read side does not have. Whether a
/// caller-chosen id should be neutralised on render at all is still open —
/// recorded as a §J2 residual in `dev-diary/lambo-for-mooshik/J-multi-client.md`
/// rather than only here, since J2 is where it comes due.
pub(super) fn conflict_err(what: &str, msg: &str, nothing: &str) -> CallToolResult {
    tracing::warn!(
        tool = what,
        conflict = %msg,
        "mcp: soft-lock conflict returned to the caller"
    );
    // I1: the same class the caller is told, unchanged from the `tool_err` path.
    note_error("conflict");
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{what}: {}; {nothing}. Wait for the expiry or work elsewhere.",
        msg.chars()
            .map(|c| if breaks_one_line(c) { ' ' } else { c })
            .collect::<String>()
    ))])
}

/// Replace any whitespace-delimited token that looks like a URL with a
/// placeholder (N3), so a warning that surfaced a store/embedder endpoint does
/// not carry it into a model-facing string. Idempotent — a redacted token has
/// no `://` left to match.
///
/// Scope (R4 nit): this matches only `scheme://…` tokens, not a bare
/// `host:port`. That is deliberate, not an oversight — no current warning path
/// emits a schemeless `host:port` (every endpoint the store/embedder logs is a
/// full URL), and a `host:port` matcher trained on a colon would over-redact
/// ordinary warning text (`ratio 3:4`, `line 42:10`, SQLSTATE-style codes),
/// corrupting the very message it is meant to keep readable. If a future warning
/// starts emitting bare `host:port`, redact it at that source (where the shape is
/// known) rather than widening this heuristic.
pub(super) fn redact_urls(s: &str) -> String {
    s.split(' ')
        .map(|tok| {
            if tok.contains("://") {
                "<redacted-url>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reject a parameter the server will not act on.
///
/// A bad parameter is the *client's* problem and it is worth surfacing where
/// the model can read and correct it, so this is a tool-level error rather than
/// a `-32602` the client renders opaquely.
pub(super) fn bad_param(msg: impl Into<String>) -> CallToolResult {
    note_error("invalid params");
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// Attach warnings to a result **where the model will actually read them**.
///
/// R1/T82-9: warnings used to live only in `structuredContent`, which MCP
/// clients treat as optional and commonly do not surface — so
/// `lambo_reserve`'s advisory-and-RAM-local warning, and `Memory::recall`'s
/// embed-failure degradation warning, reached nobody. They are now a second
/// text block. `content[0]` is deliberately left alone: for `lambo_recall` it is
/// the T5.3 context block verbatim, and that is the artifact the calling agent
/// reads.
///
/// URLs are redacted from the model-facing text (N3): a degradation warning can
/// surface a store or embedder endpoint, which the model does not need. The raw
/// warning is logged for the operator.
pub(super) fn attach_warnings(out: &mut CallToolResult, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    let mut text = String::from("warnings:");
    for w in warnings {
        tracing::debug!(warning = %w, "mcp: warning attached to a result (raw, pre-redaction)");
        text.push_str("\n- ");
        text.push_str(&redact_urls(w));
    }
    out.content.push(ContentBlock::text(text));
}

/// Piggyback settled write receipts onto a tool result (J3 shape part 3).
///
/// **Why this and not an MCP notification.** A notification lands in the
/// client's log, not in the model's context — the exact failure workstream J
/// exists to fix, where a serve refusal reached nobody. A tagged text block on
/// the next response the agent reads is the one delivery channel the model is
/// guaranteed to see.
///
/// **Per-caller through a shared hub.** The agent is the caller-asserted
/// `agent_id` of *this* call, so several clients through one hub — or through a
/// J2 proxy, which forwards the response bytes untouched — each get only their
/// own receipts. Receipts are per-agent scoped in the store as well (J1), so
/// this is a lookup, not a filter.
///
/// **Take-once**, so a settled receipt is not re-announced forever. A response
/// that never reaches its client loses its piggyback, which is why the
/// fetch-by-id surface on `lambo_stats` exists as well.
///
/// URLs are redacted (N3), and that is now the **second** layer rather than the
/// only one. Since JE2E-12 a `failed` answer carries the N4 *class* —
/// `err_class` plus "the detail was logged server-side", the same sentence
/// `tool_err` renders on the synchronous path — instead of the `LamboError`'s
/// own message. `redact_urls` stays because it is not the same guard: it matches
/// `scheme://` tokens, and the thing that reached a model before this was a
/// store **file path** ("unable to open database file: /path"), which has no
/// `://` for it to catch. Neither layer is load-bearing alone.
pub(super) fn attach_receipts(
    out: &mut CallToolResult,
    taken: &[(ReceiptId, ReceiptAnswer)],
    remaining: usize,
) {
    if taken.is_empty() {
        return;
    }
    let mut text = String::from("write receipts (your earlier writes, now settled):");
    for (id, answer) in taken {
        text.push_str("\n- ");
        text.push_str(&id.to_string());
        text.push_str(": ");
        text.push_str(&redact_urls(&answer.describe()));
    }
    if remaining > 0 {
        text.push_str(&format!(
            "\n({remaining} more settled receipt(s) will arrive on your next call)"
        ));
    }
    out.content.push(ContentBlock::text(text));
}

/// Run a tool body with panic containment (R1/T82-5).
///
/// The MCP boundary is fed arbitrary client input, and a panicking handler used
/// to drop the JSON-RPC response entirely: no result, no error, no
/// cancellation, and the caller blocked until its own timeout. T8.1 armours
/// every store attempt this way for the same reason; the same two helpers are
/// reused here so there is one panic-containment behaviour in the tree.
///
/// The panic detail goes to the log, never to the client — a payload can
/// interpolate anything, including a DSN.
pub(super) async fn contain_panic(
    tool: &'static str,
    fut: impl Future<Output = CallToolResult>,
) -> CallToolResult {
    match CatchUnwindPoll(fut).await {
        Ok(out) => out,
        Err(payload) => {
            tracing::error!(
                tool,
                panic = %panic_message(&payload),
                "mcp: tool handler panicked — contained and reported as a tool error"
            );
            // I1: a contained panic is its own outcome, not just "error".
            note_error("panic");
            CallToolResult::error(vec![ContentBlock::text(format!(
                "{tool}: internal error (the failure was logged server-side); \
                 the call had no effect beyond anything already written"
            ))])
        }
    }
}
