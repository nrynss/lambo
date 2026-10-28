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
//!    When not even the first block fits, the **concept content** is cut at
//!    a UTF-8 char boundary and marked with `…`, the rest of the block is
//!    kept, and the marker follows on its own line:
//!    ```text
//!    <content head>… [Entity, canonical] (score 0.50, id 1a2b3c4d, blast radius 3)
//!    ⚑ Load-bearing pillar — 3 nodes depend on this. Modify with caution.
//!    [truncated: 1342 tokens left out; lambo_inspect 1a2b3c4d for the full concept]
//!    ```
//!    So the label (type, canonical marker, score, id, blast radius) survives
//!    the cut. The first form that fits wins ([`Truncation`]): the whole rest
//!    of the block ([`Truncation::WithWarnings`]), then the label line
//!    without the warning lines ([`Truncation::LabelOnly`]; the warnings
//!    still reach the caller in `warnings`), then the content head alone
//!    ([`Truncation::ContentOnly`]), whose shortest form is the marker
//!    alone. `<n>` is the estimator's count for exactly the text of this
//!    block that was left out. The marker names
//!    [`ContextBlock::inspect_id`], and with a [`ContextBlock::scope`] reads
//!    `lambo_inspect <id> (scope <session>)`. Nothing follows a truncated
//!    block, so rule 1's "never skip ahead" still holds, and only the first
//!    block is ever truncated.
//! 3. **A budget too small for the marker gives an empty context.** If the
//!    marker alone does not fit, the context is `""` and no block is
//!    rendered, exactly as before #130. So `max_tokens = 0` still gives an
//!    empty context, and with the default estimator so does any budget below
//!    the marker's own size (about 22 tokens with an 8-char id).
//!
//! A recall whose first block fits renders byte-identically to the
//! pre-#130 loop: rule 2 only runs when rule 1 rendered nothing.
//!
//! ## Arithmetic
//!
//! A token count of `usize::MAX` never fits (`checked_add(1)` fails), so an
//! estimator that saturates cannot admit an unbounded block. The cut point
//! is found by binary search over the content's char boundaries that do not
//! follow whitespace (so the head never ends in whitespace and needs no
//! trimming). Every candidate is measured as the exact string returned, so
//! the result is within budget for any estimator. For an estimator that is
//! monotone in bytes (the default `ceil(bytes / 3.5)` is) the head is also
//! the longest such head that fits: each step adds at least one byte of head
//! and removes it from the left-out text, and a count that loses a digit
//! does so only after losing at least one byte, so the total never shrinks.
//!
//! Cuts are on char boundaries, not grapheme boundaries: a head may end
//! inside a combined emoji or a base letter's combining marks. That is
//! cosmetic (the string is valid UTF-8 and the marker says the rest is
//! elsewhere) and not worth a segmentation dependency.

use crate::types::RecallHit;

/// One rendered hit block, as [`fit_blocks`] takes it. Build it with
/// [`ContextBlock::for_hit`], which keeps the offsets consistent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextBlock {
    /// The block exactly as [`super::render_block`] produced it.
    pub(crate) text: String,
    /// `text[..content_len]` is the concept content (the block opens with it).
    pub(crate) content_len: usize,
    /// `text[..label_end]` is the label line; the warning lines follow.
    pub(crate) label_end: usize,
    /// The id the truncation marker names: the hit's [`super::short_id`],
    /// which `lambo_inspect` resolves as a focus.
    pub(crate) inspect_id: String,
    /// The session the hit belongs to, when it is not the caller's own (a
    /// layer hit, #33 PR 2): the marker then tells the agent to pass it as
    /// `lambo_inspect`'s `scope`. `None` renders exactly as before.
    pub(crate) scope: Option<String>,
}

impl ContextBlock {
    /// The block [`super::render_block`] renders for `hit` with its
    /// `warnings` lines, with its content and label offsets.
    pub(crate) fn for_hit(hit: &RecallHit, warnings: &[String]) -> Self {
        let text = super::render_block(hit, warnings);
        // The label line is exactly what render_block renders with no
        // warnings, and the full block starts with it. Deriving the offset
        // from that, not from the warnings' lengths, keeps it right if the
        // join format changes; if the prefix ever fails to hold, fall back
        // to the content end (the least that is still a valid slice).
        let label = super::render_block(hit, &[]);
        let label_end = if text.starts_with(&label) {
            label.len()
        } else {
            hit.content.len().min(text.len())
        };
        debug_assert!(text.starts_with(&hit.content));
        debug_assert!(text.is_char_boundary(label_end));
        Self {
            content_len: hit.content.len(),
            label_end,
            text,
            inspect_id: super::short_id(hit.node_id),
            scope: None,
        }
    }

    /// This block with `meta` items appended inside the label's closing
    /// parenthesis: `(score 0.42, id 1a2b3c4d, also 5e6f7a8b)`. With no
    /// `meta` it is unchanged. The one place label meta is inserted: #181's
    /// `also` items and #33's layer items (`from`, `also … in …`).
    pub(crate) fn with_meta(mut self, meta: &[String]) -> Self {
        if !meta.is_empty() && self.label_end > 0 && self.text[..self.label_end].ends_with(')') {
            let extra = format!(", {}", meta.join(", "));
            self.text.insert_str(self.label_end - 1, &extra);
            self.label_end += extra.len();
        }
        self
    }
}

/// How the first block was truncated, from the most kept to the least.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Truncation {
    /// Content head, then the whole rest of the block: label and every
    /// warning line are in the context.
    WithWarnings,
    /// Content head, then the label line's rest (type, canonical marker,
    /// score, id, blast radius); the warning lines are not in the context.
    LabelOnly,
    /// Content head only (possibly empty, leaving the marker alone).
    ContentOnly,
}

impl Truncation {
    /// Whether the block's label (and so its `[…, canonical]` marker) is in
    /// the context.
    pub(crate) fn keeps_label(self) -> bool {
        matches!(self, Truncation::WithWarnings | Truncation::LabelOnly)
    }

    /// Whether the block's warning lines are in the context.
    pub(crate) fn keeps_warnings(self) -> bool {
        self == Truncation::WithWarnings
    }
}

/// What [`fit_blocks`] rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FittedContext {
    /// The context text: whole blocks joined by a blank line, or one
    /// truncated block with its marker, or `""`.
    pub(crate) context: String,
    /// How many leading blocks appear whole. Hits `0..whole` are
    /// `included_in_context`.
    pub(crate) whole: usize,
    /// `Some` when the context is the first block truncated (then
    /// `whole == 0`).
    pub(crate) truncation: Option<Truncation>,
}

impl FittedContext {
    /// How many leading blocks the context renders at all, whole or
    /// truncated.
    pub(crate) fn rendered(&self) -> usize {
        self.whole + usize::from(self.truncation.is_some())
    }
}

/// The separator between two whole blocks ([`super::render_context`]).
const SEPARATOR: &str = "\n\n";

/// The mark at the content cut.
const ELLIPSIS: &str = "…";

/// The line after a truncated first block (#130). `left_out` is the
/// estimator's count for the block text left out.
pub(crate) fn truncation_marker(left_out: usize, inspect_id: &str, scope: Option<&str>) -> String {
    match scope {
        None => format!(
            "[truncated: {left_out} tokens left out; lambo_inspect {inspect_id} for the full concept]"
        ),
        Some(s) => format!(
            "[truncated: {left_out} tokens left out; lambo_inspect {inspect_id} (scope {s}) for the full concept]"
        ),
    }
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
        && let Some((text, form)) = truncate_first(first, max_tokens, &token_fn)
    {
        return FittedContext {
            context: text,
            whole: 0,
            truncation: Some(form),
        };
    }
    FittedContext {
        context,
        whole,
        truncation: None,
    }
}

/// The candidate cut points in the content: char boundaries before its end
/// that do not follow whitespace. `0` is always one.
fn content_cuts(content: &str) -> Vec<usize> {
    let mut cuts = vec![0usize];
    let mut prev_ws = false;
    for (i, ch) in content.char_indices() {
        if i > 0 && !prev_ws {
            cuts.push(i);
        }
        prev_ws = ch.is_whitespace();
    }
    cuts
}

/// The first block truncated in the first [`Truncation`] form that fits, or
/// `None` when not even the marker alone fits. Only called for a block that
/// does not fit whole.
fn truncate_first<F>(
    block: &ContextBlock,
    max_tokens: usize,
    token_fn: &F,
) -> Option<(String, Truncation)>
where
    F: Fn(&str) -> usize,
{
    let text = block.text.as_str();
    let content_len = block.content_len;
    let cuts = content_cuts(&text[..content_len]);

    let mut forms = vec![(Truncation::WithWarnings, text.len())];
    if block.label_end < text.len() {
        forms.push((Truncation::LabelOnly, block.label_end));
    }
    forms.push((Truncation::ContentOnly, content_len));

    for (form, kept_end) in forms {
        let render = |cut: usize| -> String {
            let left_out = format!("{}{}", &text[cut..content_len], &text[kept_end..]);
            let marker = truncation_marker(
                token_fn(&left_out),
                &block.inspect_id,
                block.scope.as_deref(),
            );
            if form == Truncation::ContentOnly && cut == 0 {
                marker
            } else {
                format!(
                    "{}{ELLIPSIS}{}\n{marker}",
                    &text[..cut],
                    &text[content_len..kept_end]
                )
            }
        };
        let ok = |cut: usize| fits(token_fn(&render(cut)), max_tokens);
        if !ok(cuts[0]) {
            continue;
        }
        // Invariant: cuts[lo] fits; every index >= hi does not, or is past
        // the end.
        let (mut lo, mut hi) = (0usize, cuts.len());
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if ok(cuts[mid]) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        return Some((render(cuts[lo]), form));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::format::{blast_radius_warning, default_token_count};
    use crate::types::{ConceptType, NodeId};

    fn hit(content: &str, canonical: bool) -> RecallHit {
        RecallHit {
            repo_scope: None,
            node_id: NodeId(uuid::Uuid::from_u64_pair(0x1a2b_3c4d_0000_0000, 7)),
            content: content.to_string(),
            concept_type: Some(ConceptType::Entity),
            score: 0.5,
            is_canonical: canonical,
            blast_radius: canonical.then_some(3),
        }
    }

    fn plain(content: &str) -> ContextBlock {
        ContextBlock::for_hit(&hit(content, false), &[])
    }

    /// The pre-#130 loop, verbatim in behaviour: longest prefix of whole
    /// blocks, stop at the first that does not fit. The byte-identity
    /// oracle for every budget where the first block fits.
    fn legacy(
        blocks: &[String],
        max_tokens: usize,
        token_fn: impl Fn(&str) -> usize,
    ) -> (String, usize) {
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
        (kept.join("\n\n"), kept.len())
    }

    /// The truncated text's last line, the marker.
    fn marker_of(context: &str) -> &str {
        context.rsplit_once('\n').map_or(context, |(_, m)| m)
    }

    #[test]
    fn for_hit_offsets_match_the_rendered_block() {
        let h = hit("user schema", true);
        let w = vec![blast_radius_warning(3), "Reserved by agent-b".to_string()];
        let b = ContextBlock::for_hit(&h, &w);
        assert_eq!(b.text, super::super::render_block(&h, &w));
        assert_eq!(&b.text[..b.content_len], "user schema");
        assert_eq!(&b.text[..b.label_end], super::super::render_block(&h, &[]));
        assert_eq!(b.inspect_id, super::super::short_id(h.node_id));
        assert_eq!(b.scope, None);
    }

    #[test]
    fn label_end_is_the_label_lines_end_with_warnings_and_multibyte() {
        let mut h = hit("héllo wörld 日本語 🦀 schéma", true);
        for warnings in [
            vec![],
            vec![blast_radius_warning(3)],
            vec![
                "⚑ Réserved by ägent-日本".to_string(),
                "🔥 hot: 日本語".to_string(),
                "Reserved by agent-b".to_string(),
            ],
        ] {
            for blast in [None, Some(12)] {
                h.blast_radius = blast;
                let b = ContextBlock::for_hit(&h, &warnings);
                // Release-mode checks: no debug_assert involved.
                assert!(b.text.is_char_boundary(b.label_end));
                assert!(b.text.is_char_boundary(b.content_len));
                let label = &b.text[..b.label_end];
                assert_eq!(label, super::super::render_block(&h, &[]));
                assert!(!label.contains('\n'), "label is a single line");
                assert!(label.ends_with(')'), "label ends at the meta suffix");
                let rest = &b.text[b.label_end..];
                if warnings.is_empty() {
                    assert_eq!(rest, "");
                } else {
                    assert_eq!(rest, format!("\n{}", warnings.join("\n")));
                }
            }
        }
    }

    #[test]
    fn empty_hit_list_renders_nothing() {
        for budget in [0, 1, 500, usize::MAX] {
            let f = fit_blocks(&[], budget, default_token_count);
            assert_eq!(f.context, "");
            assert_eq!((f.whole, f.truncation), (0, None));
            assert_eq!(f.rendered(), 0);
        }
    }

    #[test]
    fn first_block_that_fits_exactly_is_whole_with_no_marker() {
        let b = plain("some content");
        let budget = default_token_count(&b.text);
        let f = fit_blocks(std::slice::from_ref(&b), budget, default_token_count);
        assert_eq!(f.context, b.text);
        assert_eq!((f.whole, f.truncation), (1, None));
    }

    #[test]
    fn first_block_one_token_over_keeps_its_label_and_names_its_id() {
        let b = plain(&"y".repeat(400));
        let over = default_token_count(&b.text) - 1;
        let blocks = [b.clone(), plain("small")];
        let f = fit_blocks(&blocks, over, default_token_count);
        assert_eq!(f.truncation, Some(Truncation::WithWarnings));
        assert_eq!((f.whole, f.rendered()), (0, 1));
        assert!(default_token_count(&f.context) <= over, "{}", f.context);
        let (body, marker) = f.context.rsplit_once('\n').unwrap();
        let label_rest = &b.text[b.content_len..];
        let head = body
            .strip_suffix(&format!("{ELLIPSIS}{label_rest}"))
            .expect("content head, ellipsis, then the label kept whole");
        assert!(b.text.starts_with(head) && !head.is_empty());
        assert_eq!(
            marker,
            truncation_marker(
                default_token_count(&b.text[head.len()..b.content_len]),
                &b.inspect_id,
                None
            )
        );
        assert!(
            !f.context.contains("small"),
            "nothing follows a truncated block"
        );
    }

    #[test]
    fn a_truncated_canonical_hit_keeps_its_canonical_marker_and_warning() {
        let h = hit(&"pillar prose ".repeat(400), true);
        let w = vec![blast_radius_warning(3)];
        let b = ContextBlock::for_hit(&h, &w);
        let f = fit_blocks(std::slice::from_ref(&b), 500, default_token_count);
        assert_eq!(f.truncation, Some(Truncation::WithWarnings));
        assert!(f.context.contains("… [Entity, canonical] (score 0.50, id "));
        assert!(f.context.contains(&w[0]));
        assert!(Truncation::WithWarnings.keeps_label());
    }

    #[test]
    fn long_warnings_fall_back_to_the_label_line_then_to_the_content() {
        let h = hit(&"c".repeat(300), true);
        let w = vec!["w".repeat(2_000)];
        let b = ContextBlock::for_hit(&h, &w);
        let f = fit_blocks(std::slice::from_ref(&b), 120, default_token_count);
        assert_eq!(f.truncation, Some(Truncation::LabelOnly));
        assert!(f.context.contains("… [Entity, canonical]"));
        assert!(!f.context.contains("www"));
        assert!(default_token_count(&f.context) <= 120);

        // A budget below label + marker: the content head alone, then the
        // marker alone.
        let label_rest = &b.text[b.content_len..b.label_end];
        let marker_only_tokens = default_token_count(&truncation_marker(
            default_token_count(&format!(
                "{}{}",
                &b.text[..b.content_len],
                &b.text[b.content_len..]
            )),
            &b.inspect_id,
            None,
        ));
        let budget = marker_only_tokens + 3;
        assert!(budget < default_token_count(label_rest) + marker_only_tokens);
        let f = fit_blocks(std::slice::from_ref(&b), budget, default_token_count);
        assert_eq!(f.truncation, Some(Truncation::ContentOnly));
        assert!(f.context.starts_with('c') && f.context.contains("…\n[truncated: "));
        assert!(!Truncation::ContentOnly.keeps_label());
    }

    #[test]
    fn huge_multibyte_and_emoji_blocks_cut_on_a_char_boundary() {
        let samples = [
            "é".repeat(8_000),               // 2-byte chars, 16 KB
            "日本語のテキスト".repeat(700),  // 3-byte chars
            "🦀👩‍👩‍👧 crab family ".repeat(600), // 4-byte chars and ZWJ joins
        ];
        for content in &samples {
            let b = plain(content);
            for budget in [30, 45, 57, 500, 2_000] {
                let f = fit_blocks(std::slice::from_ref(&b), budget, default_token_count);
                assert!(f.truncation.is_some(), "budget {budget}");
                assert!(default_token_count(&f.context) <= budget, "budget {budget}");
                let marker = marker_of(&f.context);
                assert!(marker.starts_with("[truncated: "), "{marker}");
                assert!(marker.ends_with(&format!(
                    "lambo_inspect {} for the full concept]",
                    b.inspect_id
                )));
                if let Some((body, _)) = f.context.rsplit_once('\n') {
                    let head = &body[..body.find(ELLIPSIS).unwrap()];
                    assert!(content.starts_with(head), "head is a content prefix");
                    assert!(content.is_char_boundary(head.len()));
                    assert!(!head.ends_with(char::is_whitespace));
                }
            }
        }
    }

    #[test]
    fn budget_below_the_marker_gives_an_empty_context() {
        let b = plain(&"z".repeat(10_000));
        let marker_only = truncation_marker(default_token_count(&b.text), &b.inspect_id, None);
        let marker_tokens = default_token_count(&marker_only);
        for budget in [0, 1, marker_tokens - 1] {
            let f = fit_blocks(std::slice::from_ref(&b), budget, default_token_count);
            assert_eq!(f.context, "", "budget {budget}");
            assert_eq!((f.whole, f.truncation), (0, None));
        }
        // With an exact byte estimator, a budget of exactly the marker's
        // bytes is the marker alone (it still names the concept), and one
        // byte less is empty.
        let byte_marker = truncation_marker(b.text.len(), &b.inspect_id, None);
        let f = fit_blocks(std::slice::from_ref(&b), byte_marker.len(), |s: &str| {
            s.len()
        });
        assert_eq!(f.context, byte_marker);
        assert_eq!(f.truncation, Some(Truncation::ContentOnly));
        let f = fit_blocks(
            std::slice::from_ref(&b),
            byte_marker.len() - 1,
            |s: &str| s.len(),
        );
        assert_eq!(f.context, "");
    }

    #[test]
    fn max_tokens_one_is_empty() {
        let blocks = [plain("a block of ordinary length")];
        let f = fit_blocks(&blocks, 1, default_token_count);
        assert_eq!(f.context, "");
        assert_eq!(f.rendered(), 0);
    }

    #[test]
    fn second_block_that_does_not_fit_is_dropped_whole_and_nothing_after_it() {
        let blocks = [
            plain("first"),
            plain(&"second ".repeat(100)),
            plain("third"),
        ];
        // Budget fits block 1 and would fit block 3, never block 2.
        let budget = default_token_count(&format!("{}\n\n{}", blocks[0].text, blocks[2].text));
        let f = fit_blocks(&blocks, budget, default_token_count);
        assert_eq!(f.context, blocks[0].text);
        assert_eq!((f.whole, f.truncation), (1, None));
    }

    #[test]
    fn whole_blocks_are_byte_identical_to_the_pre_130_loop() {
        let blocks: Vec<ContextBlock> = (0..6)
            .map(|i| plain(&format!("concept {i} {}", "w".repeat(i * 37))))
            .collect();
        let texts: Vec<String> = blocks.iter().map(|b| b.text.clone()).collect();
        for budget in default_token_count(&texts[0])..400 {
            let f = fit_blocks(&blocks, budget, default_token_count);
            let (context, whole) = legacy(&texts, budget, default_token_count);
            assert_eq!((f.context, f.whole), (context, whole), "budget {budget}");
            assert_eq!(f.truncation, None);
        }
        for budget in texts[0].len()..900 {
            let f = fit_blocks(&blocks, budget, |s: &str| s.len());
            let (context, whole) = legacy(&texts, budget, |s: &str| s.len());
            assert_eq!((f.context, f.whole), (context, whole), "budget {budget}");
        }
    }

    #[test]
    fn saturated_estimator_never_fits() {
        let blocks = [plain("anything")];
        let f = fit_blocks(&blocks, usize::MAX, |_| usize::MAX);
        assert_eq!(f.context, "");
        assert_eq!(f.rendered(), 0);
    }

    #[test]
    fn the_head_never_ends_in_whitespace() {
        let b = plain(&format!("line one\n\n{}", "q ".repeat(300)));
        for budget in 25..120 {
            let f = fit_blocks(std::slice::from_ref(&b), budget, default_token_count);
            if let Some((body, _)) = f.context.rsplit_once('\n') {
                let head = &body[..body.find(ELLIPSIS).unwrap()];
                assert!(!head.ends_with(char::is_whitespace), "{head:?}");
            }
        }
    }

    /// L6: the binary search finds the longest head a linear scan finds,
    /// for the default estimator and an exact byte estimator, over content
    /// with whitespace runs and multibyte text.
    #[test]
    fn the_head_is_the_longest_that_fits() {
        let contents = [
            "a".repeat(700),
            "word ".repeat(150),
            format!("{}  \n\n {}", "é".repeat(90), "x y z ".repeat(80)),
            "🦀 ".repeat(200),
        ];
        let estimators: [fn(&str) -> usize; 2] = [default_token_count, |s: &str| s.len()];
        for content in &contents {
            let b = plain(content);
            let cuts = content_cuts(&b.text[..b.content_len]);
            for est in estimators {
                for budget in (20..400).step_by(7) {
                    let Some((got, form)) = truncate_first(&b, budget, &est) else {
                        continue;
                    };
                    let kept_end = match form {
                        Truncation::WithWarnings => b.text.len(),
                        Truncation::LabelOnly => b.label_end,
                        Truncation::ContentOnly => b.content_len,
                    };
                    let render = |cut: usize| {
                        let left_out =
                            format!("{}{}", &b.text[cut..b.content_len], &b.text[kept_end..]);
                        let marker = truncation_marker(est(&left_out), &b.inspect_id, None);
                        if form == Truncation::ContentOnly && cut == 0 {
                            marker
                        } else {
                            format!(
                                "{}{ELLIPSIS}{}\n{marker}",
                                &b.text[..cut],
                                &b.text[b.content_len..kept_end]
                            )
                        }
                    };
                    let best = cuts
                        .iter()
                        .copied()
                        .filter(|&c| fits(est(&render(c)), budget))
                        .max()
                        .unwrap();
                    assert_eq!(got, render(best), "budget {budget}");
                }
            }
        }
    }

    /// L7: `<n>` counts exactly the block text left out.
    #[test]
    fn the_marker_counts_exactly_the_text_left_out() {
        let b = plain(&"m".repeat(500));
        let f = fit_blocks(std::slice::from_ref(&b), 300, |s: &str| s.len());
        assert_eq!(f.truncation, Some(Truncation::WithWarnings));
        let (body, marker) = f.context.rsplit_once('\n').unwrap();
        let head_len = body.find(ELLIPSIS).unwrap();
        let left_out = b.content_len - head_len;
        assert_eq!(marker, truncation_marker(left_out, &b.inspect_id, None));
    }

    /// M3: a layer hit's marker names the scope to pass; without a scope the
    /// marker is unchanged.
    #[test]
    fn a_scoped_block_names_its_scope_in_the_marker() {
        let mut b = plain(&"s".repeat(2_000));
        let unscoped = fit_blocks(std::slice::from_ref(&b), 200, default_token_count);
        assert!(!unscoped.context.contains("scope"));
        b.scope = Some("general".into());
        let f = fit_blocks(std::slice::from_ref(&b), 200, default_token_count);
        assert!(
            marker_of(&f.context).ends_with(&format!(
                "lambo_inspect {} (scope general) for the full concept]",
                b.inspect_id
            )),
            "{}",
            f.context
        );
        assert!(default_token_count(&f.context) <= 200);
    }
}
