//! Fitting rendered hit blocks into the `max_tokens` budget (#33 PR 1).
//!
//! Every recall path that renders a context ends with the same cut: blended
//! assembly (`assemble`), the structural dispatch (`dispatch`), and, from #33
//! PR 2, the layered merge over N lists. [`fit_blocks`] is that cut, written
//! once so the paths cannot drift.
//!
//! ## The rule
//!
//! **Longest ranked prefix of whole blocks.** Blocks are taken in the order
//! given, joined by a blank line, and the **actual joined text** is measured
//! (separators included), so the rendered context is within budget. The cut
//! stops at the first block that does not fit: a lower-ranked block never
//! follows a skipped one (GPT5.6sol P1-4), even when it would fit on its own.
//!
//! ## Arithmetic
//!
//! A token count of `usize::MAX` never fits (`checked_add(1)` fails), so an
//! estimator that saturates cannot admit an unbounded block.

/// One rendered hit block, as [`fit_blocks`] takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextBlock {
    /// The block exactly as [`super::render_block`] produced it.
    pub(crate) text: String,
    /// The focus `lambo_inspect` resolves for this hit: its
    /// [`super::short_id`], or `<session>:<short id>` for a hit from a layer
    /// (#33 PR 2).
    pub(crate) inspect_id: String,
}

/// What [`fit_blocks`] rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FittedContext {
    /// The context text: whole blocks joined by a blank line, or `""`.
    pub(crate) context: String,
    /// How many leading blocks appear whole. Hits `0..whole` are
    /// `included_in_context`.
    pub(crate) whole: usize,
}

/// The separator between two whole blocks ([`super::render_context`]).
const SEPARATOR: &str = "\n\n";

/// A count fits when it is within budget and is not the saturated
/// `usize::MAX` (the original loop's overflow guard, kept as is).
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
    FittedContext { context, whole }
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

    /// The loop as it was written twice before the extraction: longest
    /// prefix of whole blocks, stop at the first that does not fit. The
    /// byte-identity oracle.
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
        }
    }

    #[test]
    fn first_block_that_fits_exactly_is_whole() {
        let text = "x".repeat(35); // ceil(35 / 3.5) = 10 tokens
        let blocks = [block(&text, "1a2b3c4d")];
        let f = fit_blocks(&blocks, 10, default_token_count);
        assert_eq!(f.context, text);
        assert_eq!(f.whole, 1);
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
        assert_eq!(f.whole, 1);
    }

    #[test]
    fn whole_blocks_are_byte_identical_to_the_original_loop() {
        let texts: Vec<String> = (0..6)
            .map(|i| format!("concept {i} {}", "w".repeat(i * 37)))
            .collect();
        let blocks: Vec<ContextBlock> = texts.iter().map(|t| block(t, "abcdef01")).collect();
        for budget in 0..400 {
            let f = fit_blocks(&blocks, budget, default_token_count);
            assert_eq!(
                f.context,
                legacy(&texts, budget, default_token_count),
                "budget {budget}"
            );
        }
        for budget in 0..600 {
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
        assert_eq!(f.whole, 0);
    }
}
