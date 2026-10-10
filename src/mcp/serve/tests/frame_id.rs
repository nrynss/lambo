//! #101 review M2: recovering the request id of a frame over the cap from
//! its head and the ring of its tail ([`IdProbe`]), and the reply built
//! from it.
//!
//! Each case runs a whole frame through the probe the way the readers do:
//! the first `head` bytes scanned as the head, the rest fed a chunk at a
//! time. A long filler between the two ends stands for the discarded middle,
//! so a member is only visible if it sits in the head or the last
//! `TAIL_BYTES` bytes.

use serde_json::{json, Value};

use crate::mcp::serve::frame_id::{
    too_large_reply, IdProbe, MAX_ID_BYTES, TAIL_BYTES, TOO_LARGE_CODE, TOO_LARGE_MESSAGE,
};

/// The discarded middle of every frame: longer than the head and the tail
/// together, so nothing in it is ever seen.
fn filler() -> String {
    "A".repeat(4 * TAIL_BYTES)
}

/// Run `frame` through a probe whose head is its first `head` bytes, fed
/// the rest in chunks of `chunk` bytes.
fn probe_with(frame: &str, head: usize, chunk: usize) -> Option<Value> {
    let bytes = frame.as_bytes();
    let head = head.min(bytes.len());
    let mut probe = IdProbe::new(&bytes[..head]);
    for part in bytes[head..].chunks(chunk.max(1)) {
        probe.feed(part);
    }
    probe.finish()
}

/// The readers' shape: a 64-byte head, 7-byte chunks (so the ring wraps
/// mid-member), and the same answer for a ring fed in one piece.
fn probe(frame: &str) -> Option<Value> {
    let chunked = probe_with(frame, 64, 7);
    assert_eq!(
        chunked,
        probe_with(frame, 64, frame.len()),
        "the answer must not depend on how the tail arrives: {frame:.120}"
    );
    chunked
}

/// A `tools/call` whose `params` holds the filler, with `before` members
/// ahead of `params` and `after` members behind it.
fn call(before: &str, after: &str) -> String {
    let filler = filler();
    format!(
        r#"{{{before}"method":"tools/call","params":{{"name":"lambo_derive","arguments":{{"x":"{filler}"}}}}{after}}}"#
    )
}

#[test]
fn an_id_before_params_is_recovered_from_the_head() {
    assert_eq!(
        probe(&call(r#""jsonrpc":"2.0","id":10,"#, "")),
        Some(json!(10))
    );
}

/// The TypeScript SDK's order: `{...request, jsonrpc, id}`.
#[test]
fn an_id_after_params_is_recovered_from_the_tail() {
    assert_eq!(
        probe(&call("", r#","jsonrpc":"2.0","id":11"#)),
        Some(json!(11))
    );
    // With whitespace and a CRLF line end.
    assert_eq!(
        probe(&format!(
            "{} \r",
            call("", r#" , "jsonrpc" : "2.0" , "id" : 12 "#)
        )),
        Some(json!(12))
    );
}

#[test]
fn a_string_id_is_recovered_and_its_escapes_decoded() {
    assert_eq!(probe(&call(r#""id":"req-7","#, "")), Some(json!("req-7")));
    assert_eq!(probe(&call("", r#","id":"a\"b\\""#)), Some(json!("a\"b\\")));
}

#[test]
fn no_id_is_none() {
    assert_eq!(probe(&call(r#""jsonrpc":"2.0","#, "")), None);
    assert_eq!(probe(&call("", r#","jsonrpc":"2.0""#)), None);
}

/// An `id` inside `params`, or inside any other nested value, is never
/// taken for the request's: not at the head, not at the tail.
///
/// Mutation: let the head scan look inside object values (or the tail scan
/// walk past an object value) and the nested id is returned.
#[test]
fn an_id_only_inside_a_nested_object_is_not_picked_up() {
    let filler = filler();
    let nested_head = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"id":99,"arguments":{{"x":"{filler}"}}}}}}"#
    );
    assert_eq!(probe(&nested_head), None);
    let nested_tail = format!(
        r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"arguments":{{"x":"{filler}"}},"id":98}}}}"#
    );
    assert_eq!(probe(&nested_tail), None);
    let nested_after =
        format!(r#"{{"method":"tools/call","params":{{"x":"{filler}"}},"meta":{{"id":97}}}}"#);
    assert_eq!(probe(&nested_after), None);
    // A string value that looks like a member is not one.
    let in_a_string =
        format!(r#"{{"method":"tools/call","params":{{"x":"{filler}"}},"note":"\",\"id\":96"}}"#);
    assert_eq!(probe(&in_a_string), None);
}

#[test]
fn an_id_that_is_not_a_clean_string_or_integer_is_none() {
    for id in [
        "null",
        "1.5",
        "1e3",
        "-0.0",
        "01",
        "true",
        "{}",
        "[1]",
        "9223372036854775808",
    ] {
        assert_eq!(
            probe(&call(&format!(r#""id":{id},"#), "")),
            None,
            "head id {id}"
        );
        assert_eq!(
            probe(&call("", &format!(r#","id":{id}"#))),
            None,
            "tail id {id}"
        );
    }
    let long = "x".repeat(MAX_ID_BYTES + 1);
    assert_eq!(probe(&call(&format!(r#""id":"{long}","#), "")), None);
    assert_eq!(
        probe(&call(r#""id":-9223372036854775808,"#, "")),
        Some(json!(i64::MIN))
    );
}

/// A response carries the *server's* id (a client answering a
/// server-initiated request); answering it would invent an error for a
/// request the client never made. So no `method`, or a `result`/`error`,
/// is no id.
///
/// Mutations: drop the `method` requirement, or the `result`/`error`
/// check, in `IdProbe::finish` and a response's id is returned.
#[test]
fn a_frame_that_is_not_a_request_is_none() {
    let filler = filler();
    let response = format!(r#"{{"jsonrpc":"2.0","id":5,"result":{{"x":"{filler}"}}}}"#);
    assert_eq!(probe(&response), None);
    let response_tail = format!(r#"{{"result":{{"x":"{filler}"}},"jsonrpc":"2.0","id":5}}"#);
    assert_eq!(probe(&response_tail), None);
    let no_method = format!(r#"{{"jsonrpc":"2.0","id":5,"params":{{"x":"{filler}"}}}}"#);
    assert_eq!(probe(&no_method), None);
    // A `result` or `error` makes it a response even beside a `method`.
    let both = format!(r#"{{"method":"x","id":5,"error":{{"x":"{filler}"}}}}"#);
    assert_eq!(probe(&both), None);
}

/// Two ids, or a head and a tail that disagree, are no id.
#[test]
fn conflicting_ids_are_none() {
    assert_eq!(probe(&call(r#""id":1,"#, r#","id":2"#)), None);
    assert_eq!(probe(&call(r#""id":1,"id":1,"#, "")), None);
}

/// A member cut off by the end of the head, or one whose start is before
/// the ring, is not trusted.
#[test]
fn a_member_cut_off_by_what_was_seen_is_none() {
    // The head stops inside the id's digits: it could go on.
    let frame = call(r#""jsonrpc":"2.0","id":123456,"#, "");
    let cut = frame.find("123").expect("id") + 2;
    assert_eq!(probe_with(&frame, cut, 7), None);
    // A string id longer than the ring is never trusted from the tail.
    let long = "y".repeat(TAIL_BYTES);
    assert_eq!(probe(&call("", &format!(r#","id":"{long}""#))), None);
    // A frame that is not an object at all (a batch).
    assert_eq!(probe(&format!("[{}]", call(r#""id":3,"#, ""))), None);
}

/// The reply: `-32600` with the fixed message, keyed to the id or to
/// `null`, and nothing else from the frame.
#[test]
fn the_reply_is_an_invalid_request_error_keyed_to_the_id_or_null() {
    let with_id: Value = serde_json::from_str(&too_large_reply(Some(&json!(7)))).expect("json");
    assert_eq!(
        with_id,
        json!({"jsonrpc":"2.0","id":7,"error":{"code":TOO_LARGE_CODE,"message":TOO_LARGE_MESSAGE}})
    );
    assert_eq!(TOO_LARGE_CODE, -32600);
    assert_eq!(TOO_LARGE_MESSAGE, "request too large (over 4 MiB)");
    let null: Value = serde_json::from_str(&too_large_reply(None)).expect("json");
    assert!(null["id"].is_null() && null.get("id").is_some(), "{null}");
    assert!(!too_large_reply(None).contains('\n'));
}
