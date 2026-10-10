//! The reply to a request frame over the size cap (#101 review M2): recover
//! the JSON-RPC request id from the bytes already seen, and answer with
//! `-32600` instead of leaving the caller to time out.
//!
//! # What is scanned
//!
//! A frame over the cap is never buffered whole, so the id is recovered from
//! the two ends of it the readers see anyway:
//!
//! * **the head**, the first `cap` bytes, which the reader already holds when
//!   the frame overflows. Serde-built clients write `id` before `params`.
//! * **the tail**, the last [`TAIL_BYTES`] bytes, kept in a fixed ring while
//!   the rest is discarded. The TypeScript SDK writes `{...request, jsonrpc,
//!   id}`, so its `id` comes after `params`, at the very end.
//!
//! # Conservative by construction
//!
//! Only a **top-level** member counts. The head is walked member by member at
//! depth one, skipping every value (strings, objects and arrays, nested to
//! any depth) without looking inside it. The tail is walked backwards from
//! the frame's closing `}` across scalar members only, and gives up at the
//! first object or array value, so it never reads a key inside `params`.
//!
//! An id is accepted only when it is a JSON string (at most [`MAX_ID_BYTES`]
//! raw bytes) or an integer that fits `i64` (rmcp's `NumberOrString`), the
//! frame has a top-level `method` (a request: a client's own *response*
//! carries the server's id, which must never be answered, as in the proxy's
//! `request_id`), and the head and tail do not disagree. Anything else,
//! including running off the end of what was seen, is "no id", and the reply
//! goes out with `"id": null` (JSON-RPC 2.0 §5).
//!
//! Nothing but the id is ever copied out of the frame, and the reply is a
//! fixed message: the frame's content is never echoed or logged.

use serde_json::Value;

/// The JSON-RPC error code of the reply: Invalid Request.
pub(crate) const TOO_LARGE_CODE: i64 = -32600;

/// The reply's message. It names the cap, so the assertion below keeps it
/// honest.
pub(crate) const TOO_LARGE_MESSAGE: &str = "request too large (over 4 MiB)";

const _: () = assert!(
    super::frames::MAX_MCP_FRAME_BYTES == 4 * 1024 * 1024,
    "TOO_LARGE_MESSAGE says 4 MiB: reword it with the cap"
);

/// How many bytes of a discarded frame's end are kept to look for an `id`
/// written after `params`: `,"jsonrpc":"2.0","id":"<128 bytes>"}` with room.
pub(crate) const TAIL_BYTES: usize = 256;

/// The longest id accepted, in raw bytes between its quotes or of its
/// digits. Longer is treated as no id.
pub(crate) const MAX_ID_BYTES: usize = 128;

/// The error line (without its newline) for a request frame over the cap,
/// keyed to `id`, or to `null` when no id was recovered.
pub(crate) fn too_large_reply(id: Option<&Value>) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(&Value::Null),
        "error": { "code": TOO_LARGE_CODE, "message": TOO_LARGE_MESSAGE },
    })
    .to_string()
}

/// What a scan learned about a frame's top-level members.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seen {
    id: IdSeen,
    /// A top-level `method` with a string value: the frame is a request or
    /// a notification.
    method: bool,
    /// A top-level `result` or `error`: the frame is a response.
    response: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
enum IdSeen {
    /// No top-level `id` in what was seen.
    #[default]
    Absent,
    /// A top-level `id` that is a clean string or `i64`.
    Found(Value),
    /// A top-level `id` that is anything else (null, a float, an object, a
    /// second `id`), so no id can be trusted.
    Unusable,
}

impl IdSeen {
    fn record(&mut self, id: Option<Value>) {
        *self = match (std::mem::take(self), id) {
            (IdSeen::Absent, Some(v)) => IdSeen::Found(v),
            _ => IdSeen::Unusable,
        };
    }
}

/// Recovers the request id of one frame over the cap: built from the head
/// the reader holds when the frame overflows, then fed every later chunk.
/// Its memory is fixed: the head's findings and a [`TAIL_BYTES`] ring.
pub(crate) struct IdProbe {
    head: Seen,
    tail: [u8; TAIL_BYTES],
    tail_len: usize,
}

impl IdProbe {
    /// Scan `head`, the frame's first bytes, and start the tail with its end.
    pub(crate) fn new(head: &[u8]) -> Self {
        let mut probe = Self {
            head: scan_head(head),
            tail: [0; TAIL_BYTES],
            tail_len: 0,
        };
        probe.feed(head);
        probe
    }

    /// The next bytes of the frame (never its newline).
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if bytes.len() >= TAIL_BYTES {
            self.tail
                .copy_from_slice(&bytes[bytes.len() - TAIL_BYTES..]);
            self.tail_len = TAIL_BYTES;
            return;
        }
        let keep = (TAIL_BYTES - bytes.len()).min(self.tail_len);
        self.tail
            .copy_within(self.tail_len - keep..self.tail_len, 0);
        self.tail[keep..keep + bytes.len()].copy_from_slice(bytes);
        self.tail_len = keep + bytes.len();
    }

    /// The id to answer, or `None` for a `null`-id reply.
    pub(crate) fn finish(&self) -> Option<Value> {
        let tail = scan_tail(&self.tail[..self.tail_len]);
        if self.head.response || tail.response || !(self.head.method || tail.method) {
            return None;
        }
        match (&self.head.id, &tail.id) {
            (IdSeen::Unusable, _) | (_, IdSeen::Unusable) => None,
            (IdSeen::Found(h), IdSeen::Found(t)) => (h == t).then(|| h.clone()),
            (IdSeen::Found(v), IdSeen::Absent) | (IdSeen::Absent, IdSeen::Found(v)) => {
                Some(v.clone())
            }
            (IdSeen::Absent, IdSeen::Absent) => None,
        }
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

/// The index just past the string that opens at `b[start]` (a `"`), or
/// `None` if it does not close within `b`.
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The index just past the value that starts at `b[i]`, or `None` if it does
/// not end within `b`. Objects and arrays are skipped whole, strings inside
/// them included, without looking at their keys.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !is_ws(b[j]) && !matches!(b[j], b',' | b'}' | b']') {
                j += 1;
            }
            // A scalar that runs to the end of what was seen may go on.
            (j > i && j < b.len()).then_some(j)
        }
    }
}

/// Whether the raw key (quotes included) is `"id"`, escapes decoded.
fn key_is(raw: &[u8], name: &str) -> bool {
    raw.len() <= 32 && serde_json::from_slice::<String>(raw).is_ok_and(|k| k == name)
}

/// The id a raw value token stands for, if it is a clean string or `i64`.
fn parse_id(raw: &[u8]) -> Option<Value> {
    match raw.first()? {
        b'"' => {
            if raw.len() > MAX_ID_BYTES + 2 {
                return None;
            }
            serde_json::from_slice::<String>(raw).ok().map(Value::from)
        }
        b'-' | b'0'..=b'9' => {
            let digits = raw.strip_prefix(b"-").unwrap_or(raw);
            let clean = !digits.is_empty()
                && digits.len() <= 19
                && digits.iter().all(u8::is_ascii_digit)
                && (digits == b"0" || digits[0] != b'0');
            if !clean {
                return None;
            }
            std::str::from_utf8(raw)
                .ok()?
                .parse::<i64>()
                .ok()
                .map(Value::from)
        }
        _ => None,
    }
}

/// Note one top-level member.
fn record(seen: &mut Seen, key: &[u8], value: &[u8]) {
    if key_is(key, "id") {
        seen.id.record(parse_id(value));
    } else if key_is(key, "method") {
        seen.method |= value.first() == Some(&b'"');
    } else if key_is(key, "result") || key_is(key, "error") {
        seen.response = true;
    }
}

/// Walk the top-level members of the object `b` starts, for as far as `b`
/// goes.
fn scan_head(b: &[u8]) -> Seen {
    let mut seen = Seen::default();
    let mut i = skip_ws(b, 0);
    if b[i..].starts_with(b"\xEF\xBB\xBF") {
        i = skip_ws(b, i + 3);
    }
    if b.get(i) != Some(&b'{') {
        return seen;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) != Some(&b'"') {
            return seen;
        }
        let Some(key_end) = string_end(b, i) else {
            return seen;
        };
        let key = &b[i..key_end];
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return seen;
        }
        i = skip_ws(b, i + 1);
        let Some(value_end) = value_end(b, i) else {
            return seen;
        };
        let after = skip_ws(b, value_end);
        // A member counts only once what follows it shows it is complete.
        match b.get(after) {
            Some(b',') => {
                record(&mut seen, key, &b[i..value_end]);
                i = after + 1;
            }
            Some(b'}') => {
                record(&mut seen, key, &b[i..value_end]);
                return seen;
            }
            _ => return seen,
        }
    }
}

fn rskip_ws(b: &[u8], mut end: usize) -> usize {
    while end > 0 && is_ws(b[end - 1]) {
        end -= 1;
    }
    end
}

/// The index of the opening quote of the string whose closing quote is
/// `b[end - 1]`, or `None` if it cannot be known from `b`. A quote is the
/// opening one when an even number of backslashes precede it; a run that
/// reaches the start of `b` could be longer, so it is not trusted.
fn rstring_start(b: &[u8], end: usize) -> Option<usize> {
    let mut p = end.checked_sub(1)?;
    while p > 0 {
        p -= 1;
        if b[p] != b'"' {
            continue;
        }
        let mut q = p;
        while q > 0 && b[q - 1] == b'\\' {
            q -= 1;
        }
        if q == 0 {
            return None;
        }
        if (p - q) % 2 == 0 {
            return Some(p);
        }
    }
    None
}

/// The start of the scalar value that ends at `b[end - 1]`, or `None` for an
/// object or array value, or one that may begin before `b` does.
fn rscalar_start(b: &[u8], end: usize) -> Option<usize> {
    let last = *b.get(end.checked_sub(1)?)?;
    if last == b'"' {
        return rstring_start(b, end);
    }
    if !(last.is_ascii_alphanumeric() || matches!(last, b'.' | b'+' | b'-')) {
        return None;
    }
    let mut s = end - 1;
    while s > 0 && (b[s - 1].is_ascii_alphanumeric() || matches!(b[s - 1], b'.' | b'+' | b'-')) {
        s -= 1;
    }
    (s > 0).then_some(s)
}

/// Walk backwards from the closing `}` at the end of `b` across top-level
/// scalar members, stopping at the first object or array value.
fn scan_tail(b: &[u8]) -> Seen {
    let mut seen = Seen::default();
    let mut end = rskip_ws(b, b.len());
    if end == 0 || b[end - 1] != b'}' {
        return seen;
    }
    end -= 1;
    loop {
        end = rskip_ws(b, end);
        let Some(value_start) = rscalar_start(b, end) else {
            return seen;
        };
        let value = &b[value_start..end];
        let colon = rskip_ws(b, value_start);
        if colon == 0 || b[colon - 1] != b':' {
            return seen;
        }
        let key_end = rskip_ws(b, colon - 1);
        if key_end == 0 || b[key_end - 1] != b'"' {
            return seen;
        }
        let Some(key_start) = rstring_start(b, key_end) else {
            return seen;
        };
        let before = rskip_ws(b, key_start);
        if before == 0 || !matches!(b[before - 1], b',' | b'{') {
            return seen;
        }
        record(&mut seen, &b[key_start..key_end], value);
        if b[before - 1] == b'{' {
            return seen;
        }
        end = before - 1;
    }
}
