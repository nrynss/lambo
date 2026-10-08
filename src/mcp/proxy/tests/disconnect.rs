//! Disconnect handling: honest errors for in-flight calls.

use super::*;

#[test]
fn a_request_gets_an_honest_error_keyed_to_its_id() {
    let reply = unreachable_reply(
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"lambo_derive"}}"#,
    )
    .expect("a request must be answered");
    let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"]["code"], HUB_UNREACHABLE_CODE);
    let msg = v["error"]["message"].as_str().unwrap();
    // What the model needs: nothing happened, it recovers, do not block.
    assert!(msg.contains("NOTHING WAS READ OR WRITTEN"), "{msg}");
    assert!(msg.contains("retry later"), "{msg}");
    assert!(msg.contains("Do not block on memory"), "{msg}");
    // N4: no path, no store URL, no raw errno text.
    assert!(!msg.contains('/'), "no socket path may leak: {msg}");
    assert!(!msg.contains("://"), "no store URL may leak: {msg}");
}

/// A string id is legal JSON-RPC and must be echoed as a string, not
/// coerced — a client matches its own id byte for byte.
#[test]
fn a_string_id_is_echoed_unchanged() {
    let reply =
        unreachable_reply(r#"{"jsonrpc":"2.0","id":"call-9","method":"tools/list"}"#).unwrap();
    let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(v["id"], "call-9");
}

/// A notification has no id, so JSON-RPC has nothing to answer and
/// inventing a response would corrupt the client's stream.
///
/// A **client response** is the third case (J2-R1-10): it carries an id and
/// no method, but that id was minted by the *holder* for a server-initiated
/// request (`sampling/createMessage`, `roots/list`). Answering it would send
/// the holder's own request id back to the client as an error the client
/// never asked for.
#[test]
fn a_notification_and_a_broken_frame_are_not_answered() {
    assert!(
        unreachable_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none()
    );
    assert!(unreachable_reply(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).is_none());
    assert!(unreachable_reply("not json at all").is_none());
    assert!(unreachable_reply("").is_none());
    // The client's own answer to a server-initiated request: an id, no
    // method. Not ours to answer.
    assert!(
        unreachable_reply(r#"{"jsonrpc":"2.0","id":11,"result":{"model":"x"}}"#).is_none(),
        "a client RESPONSE must not be answered — that id belongs to the holder"
    );
    assert!(
        unreachable_reply(r#"{"jsonrpc":"2.0","id":11,"error":{"code":-1,"message":"no"}}"#)
            .is_none()
    );
}

/// What retires an in-flight id, and what must not.
///
/// The pump answers every id still outstanding when a hub connection ends
/// (J2-R1-1), so a frame wrongly treated as an answer means a call the
/// client is still waiting on gets no error — the original defect, one step
/// removed. A `notifications/progress` echoes the request's id and is
/// emphatically not an answer.
#[test]
fn only_a_response_retires_an_in_flight_id() {
    assert_eq!(
        response_id(r#"{"jsonrpc":"2.0","id":4,"result":{"content":[]}}"#),
        Some(serde_json::json!(4))
    );
    assert_eq!(
        response_id(r#"{"jsonrpc":"2.0","id":"c-4","error":{"code":-1,"message":"x"}}"#),
        Some(serde_json::json!("c-4"))
    );
    // Progress on a call that is still running: same id, not an answer.
    assert_eq!(
        response_id(
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"id":4},"id":4}"#
        ),
        None,
        "a notification carrying the id is not the call completing"
    );
    // A server-initiated request the holder sends mid-call.
    assert_eq!(
        response_id(r#"{"jsonrpc":"2.0","id":4,"method":"roots/list"}"#),
        None
    );
    // Neither result nor error: not a response at all.
    assert_eq!(response_id(r#"{"jsonrpc":"2.0","id":4}"#), None);
    assert_eq!(response_id("not json"), None);
}

/// The two failures a proxy can have are opposite in consequence, so they
/// must not share their text or their code (J2-R1-1).
///
/// "Nothing was read or written" is true of a frame that never left this
/// process and **false** of one that reached the holder. A model told the
/// wrong one re-derives a write that may already have landed.
#[test]
fn a_lost_in_flight_call_is_told_unknown_not_nothing() {
    let lost = lost_reply(&serde_json::json!(4));
    let v: serde_json::Value = serde_json::from_str(&lost).unwrap();
    assert_eq!(v["id"], 4);
    assert_eq!(v["error"]["code"], HUB_LOST_CODE);
    assert_ne!(
        HUB_LOST_CODE, HUB_UNREACHABLE_CODE,
        "a caller must be able to tell 'did not happen' from 'unknown'"
    );
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("UNKNOWN"), "{msg}");
    assert!(
        !msg.contains("NOTHING WAS READ OR WRITTEN"),
        "the never-forwarded claim must not be reused here: {msg}"
    );
    // The one instruction that resolves the uncertainty safely.
    assert!(msg.contains("recall before re-deriving"), "{msg}");
    assert!(msg.contains("Do not block on memory"), "{msg}");
    // N4, same as the unreachable text: no path, no store URL.
    assert!(!msg.contains('/'), "no socket path may leak: {msg}");
    assert!(!msg.contains("://"), "no store URL may leak: {msg}");
}

/// `answer_lost` is per-connection: a reconnect does not retire the ids the
/// *previous* connection is still on the hook for, and answering a
/// generation twice would send the client two errors for one id.
#[tokio::test]
async fn lost_calls_are_answered_per_connection_and_only_once() {
    let mut out: Vec<u8> = Vec::new();
    let mut inflight = vec![
        (0, serde_json::json!(1)),
        (0, serde_json::json!("two")),
        (1, serde_json::json!(3)),
    ];
    let answered = HubProxy::answer_lost(&mut out, &mut inflight, 0)
        .await
        .unwrap();
    assert_eq!(answered, 2);
    assert_eq!(
        inflight,
        vec![(1, serde_json::json!(3))],
        "the surviving connection's call is still outstanding"
    );
    let text = String::from_utf8(out.clone()).unwrap();
    let ids: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["id"].clone())
        .collect();
    assert_eq!(ids, vec![serde_json::json!(1), serde_json::json!("two")]);
    // Draining the same generation again answers nothing: one error per id.
    out.clear();
    assert_eq!(
        HubProxy::answer_lost(&mut out, &mut inflight, 0)
            .await
            .unwrap(),
        0
    );
    assert!(out.is_empty());
}
