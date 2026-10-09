//! Keeping the bearer token out of an error body before it is quoted
//! (issue #21).
//!
//! The adapter quotes a non-2xx response body into its error, and those
//! errors reach logs and MCP receipts. A gateway may echo the key it refused,
//! raw, JSON-escaped inside an error object, percent-encoded from a query
//! string, or masked down to its first and last few characters. What is
//! replaced, exactly:
//!
//! * every run of at least [`MIN_ECHO_RUN`] consecutive bytes of the token
//!   (the whole token, a prefix, a suffix or any inner substring), or the
//!   whole token when it is shorter than that;
//! * the same for the token's JSON-escaped form (with and without `\/`) and
//!   its percent-encoded form (upper- and lower-case hex).
//!
//! Not detected: a run shorter than [`MIN_ECHO_RUN`] bytes (a masked echo such
//! as `sk-ab...yz`), a case-changed echo, and any other re-encoding (base64,
//! `\uXXXX` escapes of non-ASCII). The body is also cut at
//! [`QUOTED_BODY_MAX`] bytes, which bounds both the log line and the scan, and
//! the adapter downloads no more of it than [`read_cap`] allows.
//!
//! The scan is the longest-common-substring recurrence over (body byte, form
//! byte): `O(n·k)` time and `O(n + k)` memory per form, for a body of `n`
//! bytes (at most [`QUOTED_BODY_MAX`] plus the longest form) and a form of
//! `k` bytes.

use std::fmt::Write as _;

/// The shortest run of the token's bytes treated as an echo of it.
pub(super) const MIN_ECHO_RUN: usize = 8;

/// The most of a body that is quoted into an error.
pub(super) const QUOTED_BODY_MAX: usize = 8 * 1024;

/// The most of an error body worth downloading for a token of `token_len`
/// bytes: the quoted part plus the scan's look-ahead past the cut (the
/// longest token form is its percent-encoding, 3 bytes a byte) and a
/// character's worth for the boundary. Anything past it is never quoted or
/// scanned, so it is never read.
pub(super) fn read_cap(token_len: usize) -> usize {
    QUOTED_BODY_MAX
        .saturating_add(token_len.saturating_mul(3))
        .saturating_add(4)
}

/// What a scrubbed run is replaced with.
pub(super) const TOKEN_NOT_SHOWN: &str = "(token not shown)";

/// `body`, cut to [`QUOTED_BODY_MAX`] bytes, with every echo of `token` (see
/// the module doc) replaced by [`TOKEN_NOT_SHOWN`]. `None` or an empty token
/// only cuts. `truncated` says the body was read only in part, so its full
/// length is unknown.
pub(super) fn quotable_body(body: &str, token: Option<&str>, truncated: bool) -> String {
    let limit = floor_char_boundary(body, QUOTED_BODY_MAX);
    let forms = token
        .filter(|t| !t.is_empty())
        .map(token_forms)
        .unwrap_or_default();
    // Scan past the cut by the longest form, so an echo straddling the cut is
    // found whole and its part before the cut is replaced too.
    let longest = forms.iter().map(Vec::len).max().unwrap_or(0);
    let window_end = ceil_char_boundary(body, limit.saturating_add(longest));
    let window = &body.as_bytes()[..window_end];
    let mut mask = vec![false; window.len()];
    for form in &forms {
        mark_runs(window, form, &mut mask);
    }
    widen_to_char_boundaries(body, &mut mask);

    let mut out = String::with_capacity(limit);
    let mut i = 0;
    while i < limit {
        if mask[i] {
            out.push_str(TOKEN_NOT_SHOWN);
            while i < mask.len() && mask[i] {
                i += 1;
            }
        } else {
            let start = i;
            while i < limit && !mask[i] {
                i += 1;
            }
            // Both ends are char boundaries: `limit` by construction, a mask
            // edge by `widen_to_char_boundaries`.
            out.push_str(&body[start..i]);
        }
    }
    if truncated {
        out.push_str(" ... (rest of the body not shown)");
    } else if body.len() > limit {
        let _ = write!(out, " ... ({} more bytes not shown)", body.len() - limit);
    }
    out
}

/// The byte forms of `token` an echo may take: raw, JSON-escaped (with and
/// without `\/`), percent-encoded (upper- and lower-case hex). Deduplicated,
/// so a plain alphanumeric token is scanned once.
fn token_forms(token: &str) -> Vec<Vec<u8>> {
    let mut forms = vec![token.to_owned()];
    if let Ok(quoted) = serde_json::to_string(token)
        && let Some(inner) = quoted.strip_prefix('"').and_then(|q| q.strip_suffix('"'))
    {
        forms.push(inner.replace('/', "\\/"));
        forms.push(inner.to_owned());
    }
    forms.push(percent_encode(token, true));
    forms.push(percent_encode(token, false));
    forms.sort();
    forms.dedup();
    forms.into_iter().map(String::into_bytes).collect()
}

/// RFC 3986 percent-encoding of every byte outside the unreserved set.
fn percent_encode(token: &str, upper: bool) -> String {
    let mut out = String::with_capacity(token.len() * 3);
    for b in token.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else if upper {
            let _ = write!(out, "%{b:02X}");
        } else {
            let _ = write!(out, "%{b:02x}");
        }
    }
    out
}

/// Mark in `mask` every byte of `hay` inside a run of at least
/// `min(MIN_ECHO_RUN, form.len())` consecutive bytes that also occurs
/// consecutively in `form`.
fn mark_runs(hay: &[u8], form: &[u8], mask: &mut [bool]) {
    let min = MIN_ECHO_RUN.min(form.len());
    if min == 0 {
        return;
    }
    // `prev[j + 1]` / `cur[j + 1]`: the length of the longest common run
    // ending at the previous / current `hay` byte and at `form[j]`.
    let mut prev = vec![0usize; form.len() + 1];
    let mut cur = vec![0usize; form.len() + 1];
    // `ends[i]`: the longest qualifying run ending at `hay[i]`, else 0.
    let mut ends = vec![0usize; hay.len()];
    for (i, &b) in hay.iter().enumerate() {
        let mut longest = 0;
        for (j, &f) in form.iter().enumerate() {
            let run = if f == b { prev[j] + 1 } else { 0 };
            cur[j + 1] = run;
            longest = longest.max(run);
        }
        if longest >= min {
            ends[i] = longest;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    // Sweep right to left, carrying how far back the current run reaches.
    let mut reach = 0usize;
    for i in (0..hay.len()).rev() {
        reach = reach.max(ends[i]);
        if reach > 0 {
            mask[i] = true;
            reach -= 1;
        }
    }
}

/// Grow every masked span outwards to `body`'s char boundaries, so a run that
/// ends inside a multi-byte character takes the whole character and the
/// unmasked pieces stay valid UTF-8. `mask.len()` must be a char boundary.
fn widen_to_char_boundaries(body: &str, mask: &mut [bool]) {
    let n = mask.len();
    for i in 0..n {
        if mask[i] && !body.is_char_boundary(i) {
            let mut s = i;
            while !body.is_char_boundary(s) {
                s -= 1;
                mask[s] = true;
            }
        }
    }
    let mut i = 0;
    while i < n {
        if mask[i] {
            let mut e = i + 1;
            while e < n && !body.is_char_boundary(e) {
                mask[e] = true;
                e += 1;
            }
            i = e;
        } else {
            i += 1;
        }
    }
}

/// The largest char boundary of `s` at or below `at`.
fn floor_char_boundary(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The smallest char boundary of `s` at or above `at` (capped at its length).
fn ceil_char_boundary(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake token with characters JSON and percent-encoding both rewrite.
    const ODD: &str = "fake/xyzzy\"odd\\token+value=";
    /// A fake plain token.
    const PLAIN: &str = "fake-xyzzy-embed-token";

    fn scrub(body: &str, token: &str) -> String {
        quotable_body(body, Some(token), false)
    }

    #[test]
    fn the_raw_token_is_replaced_and_the_rest_kept() {
        let out = scrub(&format!("invalid key: Bearer {PLAIN} ({PLAIN})"), PLAIN);
        assert_eq!(
            out,
            format!("invalid key: Bearer {TOKEN_NOT_SHOWN} ({TOKEN_NOT_SHOWN})")
        );
    }

    /// Mutation: drop the JSON forms from `token_forms` -> red.
    #[test]
    fn a_json_escaped_echo_is_replaced() {
        let escaped = serde_json::to_string(&serde_json::json!({ "error": ODD })).unwrap();
        assert!(!escaped.contains(ODD), "the test needs a rewritten form");
        let out = scrub(&escaped, ODD);
        assert_eq!(out, format!("{{\"error\":\"{TOKEN_NOT_SHOWN}\"}}"));
        // Encoders that also escape `/`.
        let slashed = escaped.replace('/', "\\/");
        let out = scrub(&slashed, ODD);
        assert_eq!(out, format!("{{\"error\":\"{TOKEN_NOT_SHOWN}\"}}"));
    }

    /// Mutation: drop the percent-encoded forms from `token_forms` -> red.
    #[test]
    fn a_percent_encoded_echo_is_replaced() {
        for upper in [true, false] {
            let encoded = percent_encode(ODD, upper);
            assert!(encoded.contains('%'));
            let out = scrub(&format!("GET /v1?key={encoded}&x=1"), ODD);
            assert_eq!(out, format!("GET /v1?key={TOKEN_NOT_SHOWN}&x=1"), "{upper}");
        }
    }

    /// A masked echo keeps `MIN_ECHO_RUN` or more bytes of the token: the
    /// prefix, the suffix and an inner run are each replaced.
    ///
    /// Mutation: replace only exact occurrences -> red.
    #[test]
    fn a_partial_echo_of_eight_or_more_bytes_is_replaced() {
        let prefix = &PLAIN[..MIN_ECHO_RUN];
        let suffix = &PLAIN[PLAIN.len() - 9..];
        let inner = &PLAIN[4..16];
        let body = format!("key {prefix}...{suffix} near {inner}!");
        let out = scrub(&body, PLAIN);
        assert_eq!(
            out,
            format!("key {TOKEN_NOT_SHOWN}...{TOKEN_NOT_SHOWN} near {TOKEN_NOT_SHOWN}!")
        );
    }

    /// The documented limit: a run shorter than `MIN_ECHO_RUN` is not an echo.
    #[test]
    fn a_run_shorter_than_the_minimum_is_kept() {
        let short = &PLAIN[..MIN_ECHO_RUN - 1];
        let body = format!("key {short}...");
        assert_eq!(scrub(&body, PLAIN), body);
    }

    /// A token shorter than `MIN_ECHO_RUN` is replaced when whole, and its
    /// own prefix is not.
    #[test]
    fn a_short_token_is_replaced_only_whole() {
        let out = scrub("k=ab12x; k=ab12", "ab12x");
        assert_eq!(out, format!("k={TOKEN_NOT_SHOWN}; k=ab12"));
    }

    #[test]
    fn no_token_only_cuts() {
        assert_eq!(quotable_body("plain body", None, false), "plain body");
        assert_eq!(quotable_body("plain body", Some(""), false), "plain body");
    }

    /// A run that ends inside a multi-byte character takes the whole
    /// character, so the result is valid and nothing panics.
    #[test]
    fn a_run_ending_inside_a_multibyte_character_takes_it_whole() {
        let token = "fake-xyzzy-\u{e9}-token";
        // U+00C3 shares its first byte with U+00E9.
        let out = scrub("[fake-xyzzy-\u{c3}]", token);
        assert_eq!(out, format!("[{TOKEN_NOT_SHOWN}]"));
    }

    /// The body is cut at `QUOTED_BODY_MAX`, and an echo straddling the cut
    /// is replaced, not left half-shown.
    #[test]
    fn a_long_body_is_cut_and_an_echo_at_the_cut_is_still_replaced() {
        let out = quotable_body(&"a".repeat(QUOTED_BODY_MAX * 4), None, false);
        assert!(out.starts_with(&"a".repeat(QUOTED_BODY_MAX)));
        assert!(
            out.ends_with(&format!("({} more bytes not shown)", QUOTED_BODY_MAX * 3)),
            "{}",
            &out[QUOTED_BODY_MAX..]
        );

        let body = format!("{}{PLAIN} tail", "a".repeat(QUOTED_BODY_MAX - 5));
        let out = scrub(&body, PLAIN);
        assert!(!out.contains("fake"), "{}", &out[QUOTED_BODY_MAX - 10..]);
        assert!(out.contains(TOKEN_NOT_SHOWN));

        // A cut inside a multi-byte character falls back to its start.
        let body = format!("{}\u{e9}\u{e9}", "a".repeat(QUOTED_BODY_MAX - 1));
        let out = quotable_body(&body, None, false);
        assert!(out.starts_with(&"a".repeat(QUOTED_BODY_MAX - 1)));
    }
}
