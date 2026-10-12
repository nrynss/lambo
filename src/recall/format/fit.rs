//! Fitting rendered hit blocks into the `max_tokens` budget (#33 PR 1, #130).
//!
//! Every recall path that renders a context ends with the same cut: blended
//! assembly (`assemble`), the structural dispatch (`dispatch`), and, from #33
//! PR 2, the layered merge over N lists. [`fit_blocks`] is that cut, written
//! once so the paths cannot drift.
//!
//! ## The rule
//!
//! 1. **Longest ranked prefix of whole blocks.** Blocks are taken in the order
//!    given, joined by a blank line, and the **actual joined text** is
//!    measured (separators included), so the rendered context is within
//!    budget. The cut stops at the first block that does not fit: a
//!    lower-ranked block never follows a skipped one (GPT5.6sol P1-4), even
//!    when it would fit on its own.
//! 2. **A first block that is too long is truncated, not dropped (#130).**
//!    When not even the first block fits, the context is that block's head,
//!    cut at a UTF-8 boundary, followed on its own line by the marker
//!    ```text
//!    … [truncated: <n> more tokens; lambo_inspect <id> for the full concept]
//!    ```
//!    where `<n>` is the estimator's count for the omitted tail and `<id>` is
//!    the block's [`ContextBlock::inspect_id`]. Trailing whitespace of the
//!    head is trimmed so the marker never sits after a blank line (which
//!    would read as a block separator). Nothing follows a truncated block,
//!    so rule 1's "never skip ahead" still holds. Only the first block is
//!    ever truncated: once one whole block is in, every later block is
//!    whole or absent.
//! 3. **A budget too small for the marker gives an empty context.** If the
//!    marker alone (an empty head) does not fit, the context is `""` and no
//!    block is rendered, exactly as before #130. So `max_tokens = 0` still
//!    gives an empty context, and with the default estimator so does any
//!    budget below the marker's own size (a marker with an 8-char id is
//!    about 22 tokens).
//!
//! A recall whose first block fits renders byte-identically to the
//! pre-#130 loop: rule 2 only runs when rule 1 rendered nothing.
//!
//! ## Arithmetic
//!
//! A token count of `usize::MAX` never fits (`checked_add(1)` fails), so an
//! estimator that saturates cannot admit an unbounded block. The truncation
//! point is found by binary search over the head's char boundaries; every
//! candidate is measured as the exact string returned, so the result is
//! within budget for any estimator. With a monotone estimator (the default
//! `ceil(bytes / 3.5)` is) the head is also the longest that fits: growing
//! the head by one char adds at least a byte, while the marker's count
//! shrinks by at most one digit.

/// One rendered hit block, as [`fit_blocks`] takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextBlock {
    /// The block exactly as [`super::render_block`] produced it.
    pub(crate) text: String,
    /// The focus the truncation marker names, which `lambo_inspect` must
    /// resolve: the hit's [`super::short_id`], or `<session>:<short id>` for
    /// a hit from a layer (#33 PR 2).
    pub(crate) inspect_id: String,
}

/// What [`fit_blocks`] rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FittedContext {
    /// The context text: whole blocks joined by a blank line, or one
    /// truncated head with its marker, or `""`.
    pub(crate) context: String,
    /// How many leading blocks appear whole. Hits `0..whole` are
    /// `included_in_context`.
    pub(crate) whole: usize,
    /// True when the context is the first block's truncated head with the
    /// marker (then `whole == 0`).
    pub(crate) first_truncated: bool,
}

impl FittedContext {
    /// How many leading blocks the context renders at all, whole or
    /// truncated.
    pub(crate) fn rendered(&self) -> usize {
        self.whole + usize::from(self.first_truncated)
    }
}

/// The separator between two whole blocks ([`super::render_context`]).
const SEPARATOR: &str = "\n\n";

/// The marker after a truncated first block (#130). `more_tokens` is the
/// estimator's count for the omitted tail; `inspect_id` names the concept.
pub(crate) fn truncation_marker(more_tokens: usize, inspect_id: &str) -> String {
    format!(
        "… [truncated: {more_tokens} more tokens; lambo_inspect {inspect_id} for the full concept]"
    )
}

/// A count fits when it is within budget and is not the saturated
/// `usize::MAX` (the pre-#130 loop's overflow guard, kept as is).
fn fits(tokens: usize, max_tokens: usize) -> bool {
    tokens.checked_add(1).is_some() && tokens <= max_tokens
}

/// Fit `blocks` (in rank order) into `max_tokens` as measured by
/// `token_fn`. See the module docs for the rule.
pub(crate) fn fit_blocks<F>(
    blocks: &[ContextBlock],
    max_tokens: usize,
    token_fn: F,
) -> FittedContext
where
    F: Fn(&str) -> usize,
{
    let mut context = String::new();
    let mut whole = 0usize;
    for block in blocks {
        let before = context.len();
        if !context.is_empty() {
            context.push_str(SEPARATOR);
        }
        context.push_str(&block.text);
        if !fits(token_fn(&context), max_tokens) {
            context.truncate(before);
            break;
        }
        whole += 1;
    }

    if whole == 0
        && let Some(first) = blocks.first()
        && let Some(head) = truncated_head(first, max_tokens, &token_fn)
    {
        return FittedContext {
            context: head,
            whole: 0,
            first_truncated: true,
        };
    }
    FittedContext {
        context,
        whole,
        first_truncated: false,
    }
}

/// The longest head of `block` that fits with its marker, or `None` when
/// not even the marker alone fits. Only called for a block that does not
/// fit whole.
fn truncated_head<F>(block: &ContextBlock, max_tokens: usize, token_fn: &F) -> Option<String>
where
    F: Fn(&str) -> usize,
{
    let text = block.text.as_str();
    let render = |cut: usize| -> String {
        let marker = truncation_marker(token_fn(&text[cut..]), &block.inspect_id);
        let head = text[..cut].trim_end();
        if head.is_empty() {
            marker
        } else {
            format!("{head}\n{marker}")
        }
    };
    let ok = |cut: usize| fits(token_fn(&render(cut)), max_tokens);

    // Candidate cuts: every char boundary before the end (a cut at the end
    // is the whole block, which already did not fit). Cut 0 is the
    // marker-only rendering, so the list is never empty.
    let mut cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    if cuts.is_empty() {
        cuts.push(0);
    }
    if !ok(cuts[0]) {
        return None;
    }
    // Invariant: cuts[lo] fits; every index >= hi is known not to, or is
    // past the end.
    let (mut lo, mut hi) = (0usize, cuts.len());
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if ok(cuts[mid]) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(render(cuts[lo]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::format::default_token_count;

    fn block(text: &str, id: &str) -> ContextBlock {
        ContextBlock {
            text: text.to_string(),
            inspect_id: id.to_string(),
        }
    }

    /// The pre-#130 loop, verbatim in behaviour: longest prefix of whole
    /// blocks, stop at the first that does not fit. The byte-identity
    /// oracle for every budget where the first block fits.
    fn legacy(blocks: &[String], max_tokens: usize, token_fn: impl Fn(&str) -> usize) -> String {
        let mut kept: Vec<String> = Vec::new();
        for b in blocks {
            let mut provisional = kept.join("\n\n");
            if !provisional.is_empty() {
                provisional.push_str("\n\n");
            }
            provisional.push_str(b);
            let tokens = token_fn(&provisional);
            if tokens.checked_add(1).is_none() || tokens > max_tokens {
                break;
            }
            kept.push(b.clone());
        }
        kept.join("\n\n")
    }

    #[test]
    fn empty_hit_list_renders_nothing() {
        for budget in [0, 1, 500, usize::MAX] {
            let f = fit_blocks(&[], budget, default_token_count);
            assert_eq!(f.context, "");
            assert_eq!(f.whole, 0);
            assert!(!f.first_truncated);
            assert_eq!(f.rendered(), 0);
        }
    }

    #[test]
    fn first_block_that_fits_exactly_is_whole_with_no_marker() {
        let text = "x".repeat(35); // ceil(35 / 3.5) = 10 tokens
        let blocks = [block(&text, "1a2b3c4d")];
        let f = fit_blocks(&blocks, 10, default_token_count);
        assert_eq!(f.context, text);
        assert_eq!((f.whole, f.first_truncated), (1, false));
        assert!(!f.context.contains("truncated"));
    }

    #[test]
    fn first_block_one_token_over_is_truncated_with_its_id() {
        // 200 bytes = 58 tokens; budget 57.
        let text = "y".repeat(200);
        let blocks = [block(&text, "1a2b3c4d"), block("small", "5e6f7a8b")];
        let f = fit_blocks(&blocks, 57, default_token_count);
        assert!(f.first_truncated);
        assert_eq!(f.whole, 0);
        assert_eq!(f.rendered(), 1);
        assert!(default_token_count(&f.context) <= 57, "{}", f.context);
        let (head, marker) = f.context.split_once('\n').expect("head, then marker");
        assert!(text.starts_with(head) && !head.is_empty());
        let omitted = default_token_count(&text[head.len()..]);
        assert_eq!(marker, truncation_marker(omitted, "1a2b3c4d"));
        assert!(
            !f.context.contains("small"),
            "nothing follows a truncated block"
        );
        // The head is the longest that fits: one more byte does not.
        let longer = format!(
            "{}\n{}",
            &text[..head.len() + 1],
            truncation_marker(default_token_count(&text[head.len() + 1..]), "1a2b3c4d")
        );
        assert!(default_token_count(&longer) > 57);
    }

    #[test]
    fn huge_multibyte_and_emoji_blocks_cut_on_a_char_boundary() {
        let samples = [
            "é".repeat(8_000),               // 2-byte chars, 16 KB
            "日本語のテキスト".repeat(700),  // 3-byte chars
            "🦀👩‍👩‍👧 crab family ".repeat(600), // 4-byte chars and ZWJ joins
        ];
        for text in &samples {
            for budget in [30, 31, 57, 500, 2_000] {
                let blocks = [block(text, "deadbeef")];
                let f = fit_blocks(&blocks, budget, default_token_count);
                assert!(f.first_truncated, "budget {budget}");
                assert!(
                    default_token_count(&f.context) <= budget,
                    "budget {budget}: {} tokens",
                    default_token_count(&f.context)
                );
                // A valid String by construction; the head is a prefix of
                // the block at a char boundary.
                let (head, marker) = f
                    .context
                    .rsplit_once('\n')
                    .unwrap_or(("", f.context.as_str()));
                assert!(text.starts_with(head), "head is a prefix");
                assert!(text.is_char_boundary(head.len()));
                assert!(marker.starts_with("… [truncated: "), "{marker}");
                assert!(marker.ends_with("lambo_inspect deadbeef for the full concept]"));
            }
        }
    }

    #[test]
    fn budget_below_the_marker_gives_an_empty_context() {
        let text = "z".repeat(10_000);
        let blocks = [block(&text, "1a2b3c4d")];
        let marker_only = truncation_marker(default_token_count(&text), "1a2b3c4d");
        let marker_tokens = default_token_count(&marker_only);
        for budget in [0, 1, marker_tokens - 1] {
            let f = fit_blocks(&blocks, budget, default_token_count);
            assert_eq!(f.context, "", "budget {budget}");
            assert_eq!((f.whole, f.first_truncated), (0, false));
        }
        // At the marker's size something renders, and it names the concept
        // (the estimator's ceil slack may admit a byte or two of head).
        let f = fit_blocks(&blocks, marker_tokens, default_token_count);
        assert!(f.first_truncated);
        assert!(f
            .context
            .ends_with("lambo_inspect 1a2b3c4d for the full concept]"));
        // With an exact byte estimator, a budget of exactly the marker's
        // bytes is the marker alone, and one byte less is empty.
        let byte_marker = truncation_marker(text.len(), "1a2b3c4d");
        let exact = byte_marker.len();
        let f = fit_blocks(&blocks, exact, |s: &str| s.len());
        assert_eq!(f.context, byte_marker);
        let f = fit_blocks(&blocks, exact - 1, |s: &str| s.len());
        assert_eq!(f.context, "");
    }

    #[test]
    fn max_tokens_one_is_empty() {
        let blocks = [block("a block of ordinary length", "1a2b3c4d")];
        let f = fit_blocks(&blocks, 1, default_token_count);
        assert_eq!(f.context, "");
        assert_eq!(f.rendered(), 0);
    }

    #[test]
    fn second_block_that_does_not_fit_is_dropped_whole_and_nothing_after_it() {
        let blocks = [
            block("first block", "11111111"),
            block(&"second ".repeat(100), "22222222"),
            block("third", "33333333"),
        ];
        // Budget fits block 1 and would fit block 3, never block 2.
        let budget = default_token_count("first block\n\nthird");
        let f = fit_blocks(&blocks, budget, default_token_count);
        assert_eq!(f.context, "first block");
        assert_eq!((f.whole, f.first_truncated), (1, false));
        assert!(!f.context.contains("truncated"));
    }

    #[test]
    fn whole_blocks_are_byte_identical_to_the_pre_130_loop() {
        let texts: Vec<String> = (0..6)
            .map(|i| format!("concept {i} {}", "w".repeat(i * 37)))
            .collect();
        let blocks: Vec<ContextBlock> = texts.iter().map(|t| block(t, "abcdef01")).collect();
        let first = default_token_count(&texts[0]);
        for budget in first..400 {
            let f = fit_blocks(&blocks, budget, default_token_count);
            assert_eq!(
                f.context,
                legacy(&texts, budget, default_token_count),
                "budget {budget}"
            );
            assert!(!f.first_truncated);
        }
        // A byte-length estimator too.
        for budget in texts[0].len()..600 {
            let f = fit_blocks(&blocks, budget, |s: &str| s.len());
            assert_eq!(
                f.context,
                legacy(&texts, budget, |s: &str| s.len()),
                "budget {budget}"
            );
        }
    }

    #[test]
    fn saturated_estimator_never_fits() {
        let blocks = [block("anything", "1a2b3c4d")];
        let f = fit_blocks(&blocks, usize::MAX, |_| usize::MAX);
        assert_eq!(f.context, "");
        assert_eq!(f.rendered(), 0);
    }

    #[test]
    fn head_trailing_whitespace_is_trimmed_before_the_marker() {
        // A head that would end on the blank line between two lines.
        let text = format!("line one\n\n{}", "q".repeat(500));
        let blocks = [block(&text, "1a2b3c4d")];
        // Byte-length estimator, budget = "line one" + "\n" + a marker that
        // names 502 more bytes: the head stops before the blank line.
        let marker = truncation_marker(text.len() - "line one".len(), "1a2b3c4d");
        let budget = "line one".len() + 1 + marker.len();
        let f = fit_blocks(&blocks, budget, |s: &str| s.len());
        assert!(f.first_truncated);
        assert!(!f.context.contains("\n\n"), "{:?}", f.context);
        assert!(
            f.context.starts_with("line one\n… [truncated: "),
            "{:?}",
            f.context
        );
    }
}
