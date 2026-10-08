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
/// The honest caveat: the *duration* still scales with the presented length,
/// which is attacker-controlled and reveals nothing about the secret. This is
/// the same guarantee `subtle::ConstantTimeEq` gives on slices, reached without
/// adding a dependency for one comparison.
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
    let mut diff = (presented.len() ^ expected.len()) as u64;
    for (i, byte) in presented.iter().enumerate() {
        diff |= u64::from(byte ^ expected[i % expected.len()]);
    }
    std::hint::black_box(diff) == 0
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
