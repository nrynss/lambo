//! `formal/oracle/Fit.json`: Lean `Fit.fitBlocks` over `Fit.rustEnv` and
//! `Fit.rustBlock` against `fit_blocks` (`src/recall/format/fit.rs`).
//!
//! Each case names its estimator: `default` is `default_token_count` (Lean
//! `Fit.defaultTokenCount`), `bytes` the UTF-8 length, `chars` the char
//! count, `saturated` always `usize::MAX`. A block is rebuilt from its
//! content, label and warning parts, with `content_len` and `label_end`
//! their byte offsets, as `ContextBlock::for_hit` lays them out.

use serde::Deserialize;

use super::{vectors, Vectors};
use crate::recall::format::{default_token_count, fit_blocks, ContextBlock, Truncation};

/// The model this module replays (`formal/oracle/Fit.json`).
pub(super) const MODEL: &str = "Fit";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    content: String,
    label: String,
    warn: String,
    inspect_id: String,
    scope: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    blocks: Vec<Part>,
    estimator: String,
    max_tokens: usize,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Output {
    context: String,
    whole: usize,
    truncation: Option<String>,
}

fn block(p: &Part) -> ContextBlock {
    ContextBlock {
        text: format!("{}{}{}", p.content, p.label, p.warn),
        content_len: p.content.len(),
        label_end: p.content.len() + p.label.len(),
        inspect_id: p.inspect_id.clone(),
        scope: p.scope.clone(),
    }
}

fn form(t: Truncation) -> String {
    match t {
        Truncation::WithWarnings => "with_warnings",
        Truncation::LabelOnly => "label_only",
        Truncation::ContentOnly => "content_only",
    }
    .to_owned()
}

#[test]
fn fit() {
    let v = Vectors::load(MODEL, vectors!("Fit"));
    v.replay("Lambo.Fit.fitBlocks", |input: &Input| {
        let blocks: Vec<ContextBlock> = input.blocks.iter().map(block).collect();
        let est: fn(&str) -> usize = match input.estimator.as_str() {
            "default" => default_token_count,
            "bytes" => str::len,
            "chars" => |s| s.chars().count(),
            "saturated" => |_| usize::MAX,
            other => panic!("Fit.json: unknown estimator {other:?}"),
        };
        let r = fit_blocks(&blocks, input.max_tokens, est);
        Output {
            context: r.context,
            whole: r.whole,
            truncation: r.truncation.map(form),
        }
    });
}
