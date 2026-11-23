//! Property tests: [`fit_blocks`] against `formal/LamboProofs/Fit.lean`.

use std::hash::{DefaultHasher, Hash, Hasher};

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::recall::format::default_token_count;
use crate::test_util::props;

/// A block shaped like `render_block`'s: content (multi-byte chars and
/// whitespace included, so cuts land on real char boundaries), a label
/// line, and up to two warning lines.
fn block() -> impl Strategy<Value = ContextBlock> {
    (
        "[a-z é…]{0,12}",
        "[0-9a-f]{8}",
        vec("[a-z ]{0,6}", 0..3),
        proptest::option::of("[a-z]{1,4}"),
    )
        .prop_map(|(content, id, warnings, scope)| {
            let label = format!(" [Entity] (score 0.50, id {id})");
            let mut text = format!("{content}{label}");
            let label_end = text.len();
            for w in &warnings {
                text.push_str(&format!("\n\u{2691} {w}"));
            }
            ContextBlock {
                content_len: content.len(),
                label_end,
                text,
                inspect_id: id,
                scope,
            }
        })
}

/// An arbitrary estimator: a fixed pseudo-random function of the text,
/// seeded per case, that saturates to `usize::MAX` one time in ten. It is
/// not monotone in anything, which is the point: the first statements hold
/// "for any estimator".
fn arbitrary_estimator(seed: u64, range: usize) -> impl Fn(&str) -> usize {
    move |s: &str| {
        let mut h = DefaultHasher::new();
        (seed, s).hash(&mut h);
        let v = h.finish();
        if v.is_multiple_of(10) {
            usize::MAX
        } else {
            ((v / 10) % (range as u64 + 1)) as usize
        }
    }
}

/// The context the whole-block loop renders for the first `k` blocks
/// (Lean: the text `ok k` measures).
fn join(blocks: &[ContextBlock], k: usize) -> String {
    blocks[..k]
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

/// Lean `Fit.fits` (`formal/LamboProofs/Fit.lean`), transcribed rather than
/// borrowed from the code under test: within budget, and not the saturated
/// `usizeMax`.
fn spec_fits(tokens: usize, max_tokens: usize) -> bool {
    tokens < usize::MAX && tokens <= max_tokens
}

/// The budget for a case: a free draw; `usize::MAX`, where only the
/// saturation guard of `fits` refuses (the arbitrary estimator saturates one
/// time in ten); or (half the time) the measured size of prefix `k` (taken
/// modulo `blocks.len() + 1`) moved by `delta`, so a prefix that costs
/// exactly the budget, the `<=` boundary of `fits`, is common.
fn budget(
    blocks: &[ContextBlock],
    est: &impl Fn(&str) -> usize,
    (mode, k, delta, free): (u8, usize, i8, usize),
) -> usize {
    match mode {
        0 => return free,
        // The whole range: only the `usize::MAX` guard can refuse.
        3 => return usize::MAX,
        _ => {}
    }
    let size = est(&join(blocks, k % (blocks.len() + 1)));
    size.saturating_add_signed(isize::from(delta))
}

fn budget_draw() -> impl Strategy<Value = (u8, usize, i8, usize)> {
    (0u8..4, 0usize..8, -1i8..=1, 0usize..80)
}

proptest! {
    #![proptest_config(props::config())]

    /// `Fit.fit_le`, `Fit.fit_prefix_ok`, `Fit.fit_stops_at_first_miss`,
    /// `Fit.take_fit_prefix` and `Fit.fitBlocks_fits`
    /// (`formal/LamboProofs/Fit.lean`), for an arbitrary estimator, with
    /// `fits` as Lean states it ([`spec_fits`]):
    ///
    /// * `whole` is at most the number of blocks;
    /// * every non-empty prefix of `whole` blocks fits, and a whole-block
    ///   context is exactly the join of the first `whole` blocks;
    /// * if a block was left out, the prefix one longer does not fit;
    /// * a truncated first block (rule 2, #130) happens only with
    ///   `whole == 0`, and whatever is rendered fits the budget as measured
    ///   by the same estimator.
    #[test]
    fn fit_blocks_fits_and_stops_at_the_first_miss(
        blocks in vec(block(), 0..5),
        draw in budget_draw(),
        seed in any::<u64>(),
        range in 1usize..80,
    ) {
        let est = arbitrary_estimator(seed, range);
        let max_tokens = budget(&blocks, &est, draw);
        let fitted = fit_blocks(&blocks, max_tokens, &est);
        let whole = fitted.whole;
        prop_assert!(whole <= blocks.len());
        for k in 1..=whole {
            prop_assert!(spec_fits(est(&join(&blocks, k)), max_tokens), "prefix {} does not fit", k);
        }
        if whole < blocks.len() {
            prop_assert!(
                !spec_fits(est(&join(&blocks, whole + 1)), max_tokens),
                "stopped before a miss"
            );
        }
        match fitted.truncation {
            None => prop_assert_eq!(&fitted.context, &join(&blocks, whole)),
            Some(_) => {
                prop_assert_eq!(whole, 0);
                prop_assert!(spec_fits(est(&fitted.context), max_tokens));
            }
        }
        if !fitted.context.is_empty() {
            prop_assert!(spec_fits(est(&fitted.context), max_tokens));
        }
    }

    /// `Fit.fit_longest` (`formal/LamboProofs/Fit.lean`), under the
    /// downward-closed fit test the default estimator gives
    /// (`Fit.rust_default_steady`): with `default_token_count`, `whole` is
    /// the length of the longest fitting prefix of whole blocks.
    #[test]
    fn fit_blocks_keeps_the_longest_fitting_prefix_with_the_default_counter(
        blocks in vec(block(), 0..6),
        draw in budget_draw(),
    ) {
        let max_tokens = budget(&blocks, &default_token_count, draw);
        let fitted = fit_blocks(&blocks, max_tokens, default_token_count);
        for k in 0..=blocks.len() {
            let fits_k = spec_fits(default_token_count(&join(&blocks, k)), max_tokens);
            if fits_k {
                prop_assert!(k <= fitted.whole, "prefix {} fits but whole = {}", k, fitted.whole);
            } else {
                prop_assert!(k > fitted.whole, "prefix {} does not fit but whole = {}", k, fitted.whole);
            }
        }
        if fitted.truncation.is_none() {
            prop_assert_eq!(&fitted.context, &join(&blocks, fitted.whole));
        }
    }
}
