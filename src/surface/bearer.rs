//! The bearer-token check shared by every HTTP surface: `lambo serve
//! --transport http` (`crate::mcp::serve`) and the read-only web portal
//! (`crate::cli::serve_web`).
//!
//! One implementation, because the two copies diverged. T1-P3-1 flagged both
//! comparators; its remediation (`5a6f633`) rewrote only the portal's, and in
//! the opposite direction from the one `mcp::serve` documents, so the two
//! surfaces enforcing one rule disagreed about what the rule protects (#28).
//! Each surface keeps its own token type (`SecretToken`, `AuthToken`) and its
//! own error wording; only the comparison and the header parse live here.

/// Compare a presented token against the expected one without an early exit.
///
/// The accumulate-then-test shape keeps the time taken independent of *where*
/// the first differing byte falls, so a caller cannot recover the secret byte by
/// byte from response timing. Two deliberate details:
///
/// * the loop runs over the **presented** input and indexes the expected token
///   modulo its length, so a wrong-length guess does not return early and
///   thereby disclose the expected length;
/// * [`std::hint::black_box`] stops the optimiser from proving the accumulator
///   can be short-circuited.
///
/// The honest caveats:
///
/// * the *duration* scales with the presented length, which is
///   attacker-controlled and reveals nothing about the secret;
/// * the one secret-dependent operation left is the `i % expected.len()`
///   index, a division whose divisor is the secret's length. Integer division
///   has operand-dependent latency on some CPUs, so in theory its cost varies
///   with that length. It is the same for every request against one secret
///   and is not a practical leak; any wrap-around formulation (a counter
///   compared against the length, a mask) still reads the length, so the
///   division stays.
///
/// This is *stronger* on length than `subtle::ConstantTimeEq` on slices, which
/// returns early when the lengths differ and so discloses the secret's length;
/// here a wrong-length guess costs the same per byte as a right-length one.
///
/// # Why the loop is over the presented input (#28)
///
/// The portal's copy looped over the **expected** token instead, so that "the
/// input cannot leak its length through the loop count" (T1-P3-1's
/// remediation). The input's length is the one thing the caller already knows;
/// what that loop made observable is the iteration count fixed by the
/// **secret's** length. Looping over the presented input is the direction that
/// keeps the secret's length out of the timing, which is what this function
/// exists to do. Results are identical either way: `true` only for a
/// byte-equal, equal-length token.
pub(crate) fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    if expected.is_empty() {
        // Unreachable via the surfaces' token constructors, which reject empty
        // tokens; a belt-and-braces guard so the `%` below cannot divide by
        // zero.
        return false;
    }
    std::hint::black_box(fold_diff(presented, expected, || {})) == 0
}

/// The comparison loop behind [`tokens_match`]: OR together the length
/// difference and every byte difference, visiting each **presented** byte
/// once.
///
/// `on_step` runs once per loop iteration and does nothing in production
/// (`|| {}` inlines away). It exists so the unit tests can count iterations
/// and pin the property this module is for: the loop count equals the
/// presented length for every secret length. A change that iterates over
/// `expected` instead (the direction #28 removed) keeps every boolean result
/// and fails that count. `expected` must be non-empty; [`tokens_match`]
/// guarantees it.
fn fold_diff(presented: &[u8], expected: &[u8], mut on_step: impl FnMut()) -> u64 {
    let mut diff = (presented.len() ^ expected.len()) as u64;
    for (i, byte) in presented.iter().enumerate() {
        on_step();
        diff |= u64::from(byte ^ expected[i % expected.len()]);
    }
    diff
}

/// The longest bearer credential any surface compares (#32 PR 5 review L2):
/// 4 KiB, far above any real token.
///
/// A presented credential over it is refused before the comparison, so an
/// unauthenticated caller cannot buy `credentials × header length` of
/// comparison work per request with a header near hyper's limit. The cap is
/// a constant, independent of every secret, so refusing on it reveals
/// nothing; a configured secret over it could never be presented, and the
/// serve refuses one at startup.
pub(crate) const MAX_BEARER_CREDENTIAL_BYTES: usize = 4 * 1024;

/// Check a configured bearer token (a secret this process will compare
/// presented credentials against), the one rule `lambo serve` and
/// `lambo serve-web` both apply at startup (#4 PR 2 review M2).
///
/// Refused, in this order, each with a message that never quotes the
/// token:
/// - empty or whitespace-only: almost always an unset variable that
///   expanded to nothing, and accepting it would authenticate every request
///   that sends `Authorization: Bearer ` (fail closed, not silently);
/// - leading or trailing whitespace: [`bearer_credential`] trims what a
///   request presents, so such a token never matches (a trailing newline
///   or space from an env file, say; #32 PR 5 review L3);
/// - a byte outside printable ASCII (0x20 to 0x7E; a space inside the
///   token is presentable): a header value holding one is unreadable to the
///   guard, so it never matches either;
/// - longer than [`MAX_BEARER_CREDENTIAL_BYTES`]: every presented
///   credential over it is refused unread (#32 PR 5 review L2).
///
/// Each of the last three used to start a server that answered every
/// request 401 with no hint why.
pub(crate) fn check_configured_token(raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Err(
            "auth token is empty — pass a non-empty secret, or omit it entirely to \
             run unauthenticated on loopback"
                .into(),
        );
    }
    if raw.trim() != raw {
        return Err(
            "auth token has leading or trailing whitespace, which a request cannot \
             carry (the presented credential is trimmed); remove it"
                .into(),
        );
    }
    if raw.bytes().any(|b| !(0x20..=0x7e).contains(&b)) {
        return Err(
            "auth token contains a character outside printable ASCII, which an HTTP \
             Authorization header cannot carry"
                .into(),
        );
    }
    if raw.len() > MAX_BEARER_CREDENTIAL_BYTES {
        return Err(format!(
            "auth token is longer than {MAX_BEARER_CREDENTIAL_BYTES} bytes, so no request \
             could present it"
        ));
    }
    Ok(())
}

/// The `Authorization` value a request presents: exactly one header, or
/// none (#32 PR 5 review I2, shared with the portal by #4 PR 2 review L3).
///
/// A request carrying two is presented as `Some("")`, which no credential
/// matches, so it is refused like a wrong token: reading only the first
/// would let a proxy that appends its own header, or one that keeps the
/// last, disagree with the surface about who is calling. A value that is
/// not visible ASCII is unreadable, and presented as none.
pub(crate) fn presented_authorization(headers: &axum::http::HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(axum::http::header::AUTHORIZATION).iter();
    let first = values.next();
    if values.next().is_some() {
        Some("")
    } else {
        first.and_then(|v| v.to_str().ok())
    }
}

/// The credential an `Authorization` header value carries, if it is a
/// bearer one.
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1) and surrounding
/// whitespace is trimmed; anything else (no header, another scheme, a scheme
/// with nothing after it) is `None`. The credential is returned as sent, for
/// a constant-time comparison by the caller.
pub(crate) fn bearer_credential(header: Option<&str>) -> Option<&str> {
    let (scheme, credential) = header?.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| credential.trim())
}

/// Which of several expected tokens `presented` is, compared against
/// **every** one with no early exit (#32 design §6.1).
///
/// A serve configured with several credentials must not let response timing
/// say which one a guess nearly matched, or how far down the list the match
/// was: each comparison is [`tokens_match`] (constant in the secret), every
/// entry is compared whether or not an earlier one matched, and the winning
/// index is folded in with a mask rather than a branch. The time taken
/// therefore depends on the presented length, which the caller controls,
/// and on the number of credentials (#32 PR 5 review I1). That number is
/// measurable from outside, through the per-byte slope, and is not a
/// secret: it is configuration, and it says nothing about any token or
/// about which credential a guess came near.
///
/// At most one entry can match when the tokens are distinct, which the
/// credential resolver guarantees; were two equal, the last would win.
pub(crate) fn match_any<'a>(
    presented: &[u8],
    expected: impl IntoIterator<Item = &'a [u8]>,
) -> Option<usize> {
    scan(presented, expected, || {})
}

/// The loop behind [`match_any`]. `on_compare` runs once per comparison and
/// does nothing in production; the unit tests count with it to pin that the
/// scan visits every entry whichever one matches.
fn scan<'a>(
    presented: &[u8],
    expected: impl IntoIterator<Item = &'a [u8]>,
    mut on_compare: impl FnMut(),
) -> Option<usize> {
    // `usize::MAX` is "no match": no list is that long.
    let mut found = usize::MAX;
    for (i, candidate) in expected.into_iter().enumerate() {
        on_compare();
        let hit = std::hint::black_box(tokens_match(presented, candidate));
        // All ones on a hit, zero otherwise: select `i` without branching.
        let mask = usize::from(hit).wrapping_neg();
        found = (found & !mask) | (i & mask);
    }
    (std::hint::black_box(found) != usize::MAX).then_some(found)
}

#[cfg(test)]
mod tests {
    use super::{bearer_credential, fold_diff, match_any, scan, tokens_match};

    /// Does an `Authorization` header value carry the expected bearer
    /// token? The parse and the comparison, composed as both surfaces
    /// compose them (through `SessionAuthority::authenticate` since #4
    /// PR 2, which moved the portal off the single-token helper).
    fn bearer_ok(header: Option<&str>, expected: &[u8]) -> bool {
        bearer_credential(header)
            .is_some_and(|credential| tokens_match(credential.as_bytes(), expected))
    }

    /// Every acceptance case in one table: `true` only for a byte-equal,
    /// equal-length, non-empty token.
    #[test]
    fn tokens_match_accepts_only_an_exact_equal_length_token() {
        let cases: &[(&[u8], &[u8], bool, &str)] = &[
            (b"s3cret", b"s3cret", true, "equal"),
            (b"", b"s3cret", false, "empty presented token"),
            (b"s3cret", b"", false, "empty secret"),
            (b"", b"", false, "both empty"),
            (b"s3cr", b"s3cret", false, "exact prefix of the secret"),
            (b"s3cret-extra", b"s3cret", false, "secret plus a suffix"),
            (
                b"s3crets3cret",
                b"s3cret",
                false,
                "secret repeated (the % wraps)",
            ),
            (b"x3cret", b"s3cret", false, "first byte differs"),
            (b"s3crex", b"s3cret", false, "last byte differs"),
            (b"S3CRET", b"s3cret", false, "credential is case-sensitive"),
            (
                "s3cr\u{e9}t".as_bytes(),
                "s3cr\u{e9}t".as_bytes(),
                true,
                "non-ASCII equal",
            ),
            (
                "s3cr\u{e9}t".as_bytes(),
                "s3cr\u{e8}t".as_bytes(),
                false,
                "non-ASCII differs",
            ),
            (
                "s3cr\u{e9}t".as_bytes(),
                b"s3cret",
                false,
                "non-ASCII vs ASCII",
            ),
        ];
        for (presented, expected, want, label) in cases {
            assert_eq!(tokens_match(presented, expected), *want, "{label}");
        }
    }

    /// The header parse: scheme case-insensitive, surrounding whitespace
    /// trimmed, credential compared exactly.
    #[test]
    fn bearer_ok_parses_the_scheme_and_trims_whitespace() {
        let secret = b"s3cret".as_slice();
        let cases: &[(Option<&str>, bool, &str)] = &[
            (Some("Bearer s3cret"), true, "canonical"),
            (Some("bearer s3cret"), true, "lower-case scheme"),
            (Some("BEARER s3cret"), true, "upper-case scheme"),
            (Some("  Bearer s3cret  "), true, "outer whitespace"),
            (
                Some("Bearer  s3cret"),
                true,
                "double space before credential",
            ),
            (Some("Bearer\ts3cret"), false, "tab is not the separator"),
            (None, false, "missing header"),
            (Some(""), false, "empty header"),
            (Some("Bearer"), false, "scheme only"),
            (Some("Bearer "), false, "scheme and space only"),
            (Some("s3cret"), false, "no scheme"),
            (Some("Basic s3cret"), false, "wrong scheme"),
            (Some("Bearer s3cre"), false, "short credential"),
            (Some("Bearer s3cret x"), false, "trailing token"),
        ];
        for (header, want, label) in cases {
            assert_eq!(bearer_ok(*header, secret), *want, "{label}");
        }
        assert!(
            !bearer_ok(Some("Bearer x"), b""),
            "empty secret never matches"
        );
    }

    /// The property #28 fixed: the loop visits each presented byte exactly
    /// once, whatever the secret's length, so the iteration count never
    /// follows the secret. Iterating over `expected` (T1-P3-1's direction)
    /// keeps every boolean above and fails here, because the count would
    /// track the secret's length instead.
    #[test]
    fn the_loop_count_follows_the_presented_length_not_the_secret() {
        let presented_lengths = [0usize, 1, 5, 16, 33];
        let secret_lengths = [1usize, 4, 16, 64];
        for &p in &presented_lengths {
            for &e in &secret_lengths {
                let presented = vec![b'a'; p];
                let expected = vec![b'b'; e];
                let mut steps = 0usize;
                fold_diff(&presented, &expected, || steps += 1);
                assert_eq!(
                    steps, p,
                    "presented len {p}, secret len {e}: the loop must run once per presented byte"
                );
            }
        }
    }

    /// The fold is the whole decision: zero exactly when [`tokens_match`]
    /// says yes, so the counted loop is the production loop.
    #[test]
    fn fold_diff_is_zero_exactly_for_a_match() {
        assert_eq!(fold_diff(b"s3cret", b"s3cret", || {}), 0);
        assert_ne!(fold_diff(b"s3cr", b"s3cret", || {}), 0);
        assert_ne!(fold_diff(b"s3crex", b"s3cret", || {}), 0);
        assert_ne!(fold_diff(b"s3crets3cret", b"s3cret", || {}), 0);
    }

    /// #32 §6.1: the scan answers which credential matched, or none.
    #[test]
    fn match_any_finds_the_one_matching_entry() {
        let secrets: [&[u8]; 3] = [b"first-secret", b"second-secret", b"third-secret"];
        let cases: &[(&[u8], Option<usize>, &str)] = &[
            (b"first-secret", Some(0), "first"),
            (b"second-secret", Some(1), "middle"),
            (b"third-secret", Some(2), "last"),
            (b"second-secre", None, "a prefix of one entry"),
            (b"second-secrets", None, "one entry plus a suffix"),
            (b"", None, "empty"),
            (b"unrelated", None, "no entry"),
        ];
        for (presented, want, label) in cases {
            assert_eq!(match_any(presented, secrets), *want, "{label}");
        }
        assert_eq!(match_any(b"anything", []), None, "no credentials");
        assert_eq!(match_any(b"x", [b"".as_slice()]), None, "an empty secret");
    }

    /// The property the scan exists for: every entry is compared whichever
    /// one matches, and when none does, so the number of comparisons never
    /// says where in the list a token sits. An early `return Some(i)` keeps
    /// every answer above and fails here.
    #[test]
    fn the_scan_compares_every_entry_whatever_matches() {
        let secrets: [&[u8]; 4] = [b"aaaa", b"bbbb", b"cccc", b"dddd"];
        for presented in [b"aaaa".as_slice(), b"cccc", b"dddd", b"zzzz", b""] {
            let mut compares = 0usize;
            scan(presented, secrets, || compares += 1);
            assert_eq!(compares, secrets.len(), "{presented:?}");
        }
    }

    /// The header parse on its own: the credential as sent, or `None`.
    #[test]
    fn bearer_credential_returns_the_credential_of_a_bearer_header() {
        assert_eq!(bearer_credential(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_credential(Some(" bearer  abc ")), Some("abc"));
        assert_eq!(bearer_credential(Some("Basic abc")), None);
        assert_eq!(bearer_credential(Some("Bearer")), None);
        assert_eq!(bearer_credential(None), None);
    }
}
