//! Property tests: [`length_chunks`] against
//! `formal/LamboProofs/BatchQueue.lean`.

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::test_util::props;

/// Lean `BatchQueue.lengthChunks Cfg.rust` (`formal/LamboProofs/BatchQueue.lean`),
/// transcribed: a stable sort of the indices by length (`sortBy`), then
/// `chunksOf`, which carries the open chunk and its first index and closes
/// the chunk (`lcAux`) only when it is `FORWARD_WIDTH` wide or the next
/// length exceeds twice its first one plus `CHUNK_SLACK_BYTES`. Lean proves
/// this equal to the loop as the Rust writes it (`chunksOf_eq_lit`).
fn spec_length_chunks(lens: &[usize]) -> Vec<Vec<usize>> {
    fn lc_aux(lens: &[usize], cur: Vec<usize>, h: usize, rest: &[usize]) -> Vec<Vec<usize>> {
        match rest.split_first() {
            None => vec![cur],
            Some((&i, is)) => {
                if cur.len() < 8 && lens[i] <= 2 * lens[h] + 64 {
                    let mut cur = cur;
                    cur.push(i);
                    lc_aux(lens, cur, h, is)
                } else {
                    let mut out = vec![cur];
                    out.extend(lc_aux(lens, vec![i], i, is));
                    out
                }
            }
        }
    }
    // `sortBy`: insertion after every index whose length is at most its own.
    let mut order: Vec<usize> = Vec::new();
    for i in 0..lens.len() {
        let at = order
            .iter()
            .position(|&j| lens[i] < lens[j])
            .unwrap_or(order.len());
        order.insert(at, i);
    }
    match order.split_first() {
        None => Vec::new(),
        Some((&i, is)) => lc_aux(lens, vec![i], i, is),
    }
}

proptest! {
    #![proptest_config(props::config())]

    /// `BatchQueue.lengthChunks_perm`, `BatchQueue.lengthChunks_width`,
    /// `BatchQueue.lengthChunks_ne_nil`, `BatchQueue.lengthChunks_sorted` and
    /// `BatchQueue.chunksOf_slack` (`formal/LamboProofs/BatchQueue.lean`,
    /// `Cfg.rust`: width `FORWARD_WIDTH`, slack `CHUNK_SLACK_BYTES`), on
    /// random text lengths:
    ///
    /// * every text is in exactly one chunk;
    /// * no chunk is empty or wider than `FORWARD_WIDTH`;
    /// * in chunk order the lengths never decrease;
    /// * within a chunk every text is at most twice any other plus
    ///   `CHUNK_SLACK_BYTES`;
    /// * the chunks are exactly Lean's (`BatchQueue.chunksOf_eq_lit`): a
    ///   chunk closes only when full or when the next text is too long for
    ///   it, so the rule above never splits more than it must.
    #[test]
    fn length_chunks_place_every_text_once_within_the_caps(
        lens in vec(prop_oneof![0usize..200, 0usize..5_000], 0..40)
    ) {
        let chunks = length_chunks(&lens);
        prop_assert_eq!(&chunks, &spec_length_chunks(&lens), "not Lean lengthChunks");
        // Cfg.rust: the constants the transcription above spells out.
        prop_assert_eq!((FORWARD_WIDTH, CHUNK_SLACK_BYTES), (8, 64));
        let mut all: Vec<usize> = chunks.concat();
        let order = all.clone();
        all.sort_unstable();
        prop_assert_eq!(all, (0..lens.len()).collect::<Vec<_>>());
        for ch in &chunks {
            prop_assert!(!ch.is_empty() && ch.len() <= FORWARD_WIDTH, "{:?}", ch);
            for &a in ch {
                for &b in ch {
                    prop_assert!(lens[b] <= 2 * lens[a] + CHUNK_SLACK_BYTES, "{:?} in {:?}", (a, b), ch);
                }
            }
        }
        prop_assert!(order.windows(2).all(|w| lens[w[0]] <= lens[w[1]]));
    }
}
