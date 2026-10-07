//! Request forwarding: the handshake replay and frame reading.

use super::*;

/// The handshake record keeps the client's own frames **verbatim** — the
/// replay must be byte-identical, because a rebuilt initialize that differs
/// from the one the client sent negotiates a different session than the one
/// the client believes it has.
#[test]
fn the_handshake_records_the_clients_own_frames_and_nothing_else() {
    let mut h = Handshake::default();
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
    let inited = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    h.observe(init);
    h.observe(inited);
    // A tool call is traffic, not session state; recording it would replay a
    // call the client already made.
    h.observe(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_recall"}}"#);
    h.observe("not json");
    assert_eq!(h.initialize.as_deref(), Some(init));
    assert_eq!(h.initialized.as_deref(), Some(inited));
}

/// Before the client has handshaken there is nothing to rebuild, and the
/// replay must be a no-op rather than an invented `initialize` — the
/// client's own is about to arrive.
///
/// Driven rather than asserted (J2-R1-15): the previous version of this test
/// never called `replay`, it checked that `Handshake::default()` has two
/// `None`s. What matters is that **no bytes reach the holder**, which is a
/// property of `replay`, not of the struct.
#[tokio::test]
async fn a_handshake_that_never_happened_replays_nothing() {
    let h = Handshake::default();
    let mut read = BufReader::new(tokio::io::empty());
    let mut wrote: Vec<u8> = Vec::new();
    let before = h.replay(&mut read, &mut wrote).await.expect("a no-op");
    assert!(
        wrote.is_empty(),
        "an un-handshaken client must send nothing on reconnect, not an invented initialize"
    );
    assert!(before.is_empty());
    // And it must not have tried to read an answer either: `empty()` is at
    // EOF, so a swallow would have failed rather than returned Ok.
}

/// The replay finds its answer by **id**, not by position (J2-R1-12).
///
/// A holder that says anything before answering used to have that frame
/// swallowed and its actual `initialize` response forwarded to the client as
/// a duplicate answer to an id the client already holds. The preamble is now
/// returned for the caller to forward, and only the response is dropped.
#[tokio::test]
async fn the_replay_swallows_the_initialize_response_and_forwards_what_came_before_it() {
    let mut h = Handshake::default();
    h.observe(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    h.observe(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let holder = concat!(
        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"h"}}}"#,
        "\n",
    );
    let mut read = BufReader::new(std::io::Cursor::new(holder.as_bytes().to_vec()));
    let mut wrote: Vec<u8> = Vec::new();
    let before = h.replay(&mut read, &mut wrote).await.expect("replayed");
    assert_eq!(
        before.len(),
        1,
        "the notification must be forwarded: {before:?}"
    );
    assert!(before[0].contains("notifications/message"));
    // Both recorded frames went out, byte-identical and in order.
    let sent = String::from_utf8(wrote).unwrap();
    let sent: Vec<&str> = sent.lines().collect();
    assert_eq!(
        sent,
        vec![
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ]
    );
}

/// J2-R1-8: a holder that accepts and never answers must not park the pump.
///
/// `UnixStream::connect` succeeds as soon as the connection lands in the
/// listener's backlog, so this needs no hostile peer — a holder whose accept
/// loop is starved is enough. The unbounded `read_line` this replaces made
/// the process deaf to SIGTERM too, because the replay is awaited inside a
/// `select!` arm **body**.
///
/// Time is paused, so the 2s budget costs the suite nothing: the runtime
/// auto-advances the clock the moment the read is the only pending work,
/// which is exactly the state a never-answering holder produces.
#[tokio::test(start_paused = true)]
async fn a_holder_that_never_answers_the_replay_is_given_up_on() {
    let mut h = Handshake::default();
    h.observe(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    // The far half is held open and silent for the whole call.
    let (ours, _theirs) = tokio::io::duplex(4096);
    let mut read = BufReader::new(ours);
    let mut wrote: Vec<u8> = Vec::new();
    let err = h
        .replay(&mut read, &mut wrote)
        .await
        .expect_err("a silent holder must not be waited on forever");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    assert!(
        err.to_string()
            .contains("did not answer the replayed initialize"),
        "the operator needs to know which holder behaviour this was: {err}"
    );
}

/// J2-R1-4, the finding that mattered most of the three framing ones: a
/// holder that dies mid-write must not have the half-object that reached the
/// socket delivered to the client as a frame.
#[tokio::test]
async fn a_torn_final_frame_is_dropped_not_forwarded() {
    let torn = br#"{"jsonrpc":"2.0","id":1,"resu"#.to_vec();
    let mut read = BufReader::new(std::io::Cursor::new(torn));
    assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Torn(29));
    // And then end-of-stream, so the pump reports Closed exactly once.
    assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Eof);
}

/// A complete frame, `\r\n` framing, and an empty line all behave; an EOF on
/// a frame boundary is an EOF and nothing else.
#[tokio::test]
async fn a_complete_frame_is_read_without_its_newline() {
    let bytes = b"{\"a\":1}\n{\"b\":2}\r\n\n".to_vec();
    let mut read = BufReader::new(std::io::Cursor::new(bytes));
    assert_eq!(
        read_frame(&mut read).await.unwrap(),
        Framed::Line(r#"{"a":1}"#.to_string())
    );
    assert_eq!(
        read_frame(&mut read).await.unwrap(),
        Framed::Line(r#"{"b":2}"#.to_string()),
        "a CRLF-framed peer must not leave a stray carriage return in the frame"
    );
    assert_eq!(
        read_frame(&mut read).await.unwrap(),
        Framed::Line(String::new())
    );
    assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Eof);
}

/// J2-R1-17: one bad byte used to end the pump, and the client was told
/// "proxy client disconnected" for what was a decode failure. The frame is
/// dropped and the **stream survives**.
#[tokio::test]
async fn a_non_utf8_frame_is_dropped_and_the_stream_survives() {
    let mut bytes = b"{\"a\":\"".to_vec();
    bytes.push(0xff);
    bytes.extend_from_slice(b"\"}\n{\"b\":2}\n");
    let mut read = BufReader::new(std::io::Cursor::new(bytes));
    assert!(matches!(
        read_frame(&mut read).await.unwrap(),
        Framed::NotUtf8(_)
    ));
    assert_eq!(
        read_frame(&mut read).await.unwrap(),
        Framed::Line(r#"{"b":2}"#.to_string()),
        "the next frame must still arrive: a bad frame is not end-of-input"
    );
}

/// J2-R1-18: neither direction may grow a buffer without bound. The
/// over-long frame is discarded **through its newline**, so the stream
/// resynchronises instead of splitting the oversize frame into garbage
/// frames.
#[tokio::test]
async fn an_oversize_frame_is_dropped_and_the_stream_resynchronises() {
    let mut bytes = vec![b'x'; MAX_FRAME_BYTES + 10];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"after\":1}\n");
    let mut read = BufReader::new(std::io::Cursor::new(bytes));
    match read_frame(&mut read).await.unwrap() {
        Framed::Oversize(bytes) => assert_eq!(bytes, MAX_FRAME_BYTES + 10),
        other => panic!("expected Oversize, got {other:?}"),
    }
    assert_eq!(
        read_frame(&mut read).await.unwrap(),
        Framed::Line(r#"{"after":1}"#.to_string())
    );
}
