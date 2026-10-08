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

/// Does an `Authorization` header value carry the expected bearer token?
///
/// The scheme is matched case-insensitively (RFC 7235 §2.1); the credential
/// itself is compared byte-for-byte in constant time by [`tokens_match`].
pub(crate) fn bearer_ok(header: Option<&str>, expected: &[u8]) -> bool {
    let Some(raw) = header else {
        return false;
    };
    let raw = raw.trim();
    let Some((scheme, credential)) = raw.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    tokens_match(credential.trim().as_bytes(), expected)
}

#[cfg(test)]
mod tests {
    use super::{bearer_ok, fold_diff, tokens_match};

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
}
