//! What a proxy's client is told when the holder cannot answer: the honest
//! JSON-RPC errors for a call that never left this process
//! ([`unreachable_reply`]) and for one lost with the holder
//! ([`HubProxy::answer_lost`]), and the exit when the client itself is gone.

use tokio::io::AsyncWriteExt;

use super::HubProxy;
use crate::types::LamboError;

use super::forwarding::request_id;

/// JSON-RPC error code returned for a call the proxy could not forward.
///
/// In the implementation-defined `-32000..=-32099` server range, deliberately
/// not one of the reserved codes: this is not a bad request, a missing method or
/// an internal fault in the holder — it is *this process* being unable to reach
/// the holder at all, which is a distinct thing for a client to log.
///
/// **It means the call never left this process.** That is why it is a different
/// code from [`HUB_LOST_CODE`]: the two differ in whether a retry is safe, which
/// is the only thing a caller can act on.
pub(super) const HUB_UNREACHABLE_CODE: i64 = -32001;

/// JSON-RPC error code returned for a call that **was** forwarded and then lost
/// with the holder (J2-R1-1).
///
/// A separate code from [`HUB_UNREACHABLE_CODE`] on purpose. The two situations
/// are indistinguishable in the logs and opposite in consequence: an
/// unreachable holder means the call did not happen and a bare retry is safe,
/// while a lost in-flight call means *nobody knows* whether it happened and a
/// bare retry of a write may duplicate it. Collapsing them into one code would
/// force every caller to guess, and the honest answer here is "unknown", not
/// "nothing".
pub(super) const HUB_LOST_CODE: i64 = -32002;

/// What a caller is told when the holder cannot be reached.
///
/// Written for the **model**, which is who reads a tool error, and under the
/// same N4 discipline as `mcp::server`'s `tool_err`: no socket path, no store
/// URL, no raw connect error, no internal lease state. Three things a calling
/// agent can act on — nothing happened, memory returns by itself, and it is
/// safe to carry on without memory (AGENTS.md's own rule is never to block on
/// memory).
pub(super) const HUB_UNREACHABLE_MESSAGE: &str =
    "lambo: this client reaches memory through the process \
     that holds this session, and that process is not responding. NOTHING WAS READ OR WRITTEN. \
     Memory recovers on its own once a lambo serve holds the session again — the previous \
     holder's lease lapses within 45 seconds — so retry later. Do not block on memory: carry \
     on with the work and record it when memory answers again.";

/// What a caller is told when its call was already inside the holder when the
/// holder stopped answering (J2-R1-1).
///
/// Same N4 discipline as [`HUB_UNREACHABLE_MESSAGE`] — model-facing, no socket
/// path, no store URL, no errno — but deliberately **not** the same claim.
/// `HUB_UNREACHABLE_MESSAGE` says "NOTHING WAS READ OR WRITTEN" because the
/// frame never left this process. Here the frame did leave, and this process
/// cannot know what the holder did with it before it died: an embed that had
/// already committed, or one that had not. Telling a model "nothing happened"
/// in that state is a lie that costs a duplicate write, so the text says
/// *unknown* and gives the one instruction that resolves it — recall before
/// re-deriving.
pub(super) const HUB_LOST_MESSAGE: &str =
    "lambo: this call had already been handed to the process that \
     holds this session when that process stopped answering, so its outcome is UNKNOWN. It may \
     have been applied or it may not — if it was a write, treat it as neither done nor undone; \
     if it was a read, you received nothing. Memory recovers on its own once a lambo serve holds \
     the session again, so retry later. When it answers, recall before re-deriving: repeating a \
     write that did land duplicates it, and repeating one that did not is the fix. Do not block \
     on memory: carry on with the work.";

/// The `LEASE_TTL` figure quoted verbatim in [`HUB_UNREACHABLE_MESSAGE`].
///
/// Model-facing text cannot be `format!`ed into a `const`, so the number is
/// written out — and this assertion is what stops it becoming a lie if the TTL
/// ever moves (J2-R1-14). A build that changes `LEASE_TTL` fails here, at the
/// sentence that needs rewording.
const _: () = assert!(
    crate::store::lease::LEASE_TTL.as_secs() == 45,
    "HUB_UNREACHABLE_MESSAGE tells the model the previous holder's lease lapses \
     'within 45 seconds'. LEASE_TTL has changed, so that sentence is now false — \
     reword it and update this assertion."
);

/// One JSON-RPC error response, keyed to `id`.
pub(super) fn error_frame(id: &serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// Synthesize the JSON-RPC error a client gets for a call the proxy could not
/// forward — or `None` when the frame needs no answer (see `request_id`).
///
/// This is the *never left this process* case: nothing was read or written.
pub fn unreachable_reply(client_frame: &str) -> Option<String> {
    request_id(client_frame)
        .map(|id| error_frame(&id, HUB_UNREACHABLE_CODE, HUB_UNREACHABLE_MESSAGE))
}

/// The error a client gets for a call that was forwarded and then lost with the
/// holder (J2-R1-1) — the *outcome unknown* case.
pub(super) fn lost_reply(id: &serde_json::Value) -> String {
    error_frame(id, HUB_LOST_CODE, HUB_LOST_MESSAGE)
}

/// Our own client's stdout failed — the pipe is gone, so there is nobody left
/// to serve. A proxy holds no tail and no lease, so this is an ordinary exit
/// condition rather than a durability event.
pub(super) fn client_gone(e: std::io::Error) -> LamboError {
    LamboError::Config(format!("proxy client stdout: {e}"))
}

impl HubProxy {
    /// Answer every request still outstanding on `generation`, then forget it.
    ///
    /// This is the mechanism behind "never hangs" for the one call that used to
    /// hang forever (J2-R1-1). It runs when a hub connection ends — for the
    /// current connection and for a superseded one alike, because a client
    /// waiting on an id does not care which connection carried it.
    ///
    /// The reply is [`HUB_LOST_MESSAGE`], not [`HUB_UNREACHABLE_MESSAGE`]: these
    /// frames *were* written to the holder, so "nothing was read or written" is
    /// false for them and a model that believed it would re-derive a write that
    /// may already have landed.
    ///
    /// Returns how many were answered, so the caller can log the count without
    /// logging when there is nothing to say.
    pub(super) async fn answer_lost<W: AsyncWriteExt + Unpin>(
        client: &mut W,
        inflight: &mut Vec<(u64, serde_json::Value)>,
        generation: u64,
    ) -> Result<usize, LamboError> {
        let mut lost = Vec::new();
        inflight.retain(|(request_generation, id)| {
            if *request_generation == generation {
                lost.push(id.clone());
                false
            } else {
                true
            }
        });
        for id in &lost {
            Self::send(client, &lost_reply(id))
                .await
                .map_err(client_gone)?;
        }
        Ok(lost.len())
    }
}
