//! When recall shows the suspected-supersession line (#198).
//!
//! The trigger ([`crate::graph::suspected`]) records a
//! `SuspectedSupersedes` edge on every backend whatever the embedder. Owner
//! decision 1 gates only the **prose line**: it is shown for an embedder
//! whose trigger precision was measured at 0.90 or better on the #190
//! labelled set; below the bar, or unmeasured, the signal is a structured
//! field only (`possibly_superseded_by` on the recall detail, the ledger's
//! hit line), never a line in the context block or the warnings.
//!
//! `[embedder] suspected_supersession_line` chooses ([`SuspectedLine`]):
//! `validated` (the default) shows the line only for a contract in
//! [`VALIDATED_CONTRACTS`], `on` for every contract, `off` for none.

use serde::{Deserialize, Serialize};

use crate::types::EmbeddingContract;

/// `[embedder] suspected_supersession_line`: when recall shows the
/// `⚠ Possibly superseded by …` line. The structured field is present
/// whenever the pair is shown, whatever this says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuspectedLine {
    /// Only for a contract listed in [`VALIDATED_CONTRACTS`].
    #[default]
    Validated,
    /// For every contract (an operator's own measurement, or a trial).
    On,
    /// For none: structured field only.
    Off,
}

impl std::fmt::Display for SuspectedLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Validated => "validated",
            Self::On => "on",
            Self::Off => "off",
        })
    }
}

/// One embedding contract the trigger's precision was measured on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValidatedContract {
    /// [`EmbeddingContract::kind`].
    pub kind: &'static str,
    /// A prefix [`EmbeddingContract::model`] must start with, or `None` for
    /// any model string (including none).
    pub model_prefix: Option<&'static str>,
    /// [`EmbeddingContract::dim`].
    pub dim: usize,
    /// The measured held-out precision, for the record.
    pub held_out_precision: f64,
    /// Where the numbers come from.
    pub evidence: &'static str,
}

/// The contracts the line is validated for: BGE-M3 only.
///
/// Measured on the #211 in-regime pairs at cosine >= 0.85 with BGE-M3
/// (`BAAI/bge-m3@5617a9f6`, candle on CPU): precision 0.97 dev and 0.96
/// held-out (95% lower bound 0.81), recall 25/25 held-out
/// (`evidence/issue-198-trigger/`). Both BGE-M3 kinds are listed: `candle`
/// runs the Hugging Face weights in-process and `bge_m3` the same model's
/// GGUF under llama.cpp (cosine parity measured for K2). The trigger's text
/// rule does not depend on the embedder; the embedder decides only which
/// pairs reach it, through the merge threshold.
///
/// EmbeddingGemma 2 is **not** listed: its precision is unmeasured, so it
/// gets the structured field only until it is measured at 0.90 or better.
pub const VALIDATED_CONTRACTS: &[ValidatedContract] = &[
    ValidatedContract {
        kind: "candle",
        model_prefix: Some("BAAI/bge-m3@"),
        dim: 1024,
        held_out_precision: 0.96,
        evidence: "evidence/issue-198-trigger/",
    },
    ValidatedContract {
        kind: "bge_m3",
        model_prefix: None,
        dim: 1024,
        held_out_precision: 0.96,
        evidence: "evidence/issue-198-trigger/",
    },
];

/// Whether `contract` is one the line is validated for.
pub fn is_validated(contract: &EmbeddingContract) -> bool {
    VALIDATED_CONTRACTS.iter().any(|v| {
        v.kind == contract.kind
            && v.dim == contract.dim
            && v.model_prefix
                .is_none_or(|p| contract.model.as_deref().is_some_and(|m| m.starts_with(p)))
    })
}

/// Whether recall shows the prose line under `mode` for a session stamped
/// with `contract` (`None`: nothing embedded yet, so nothing validated).
pub fn shows_line(mode: SuspectedLine, contract: Option<&EmbeddingContract>) -> bool {
    match mode {
        SuspectedLine::On => true,
        SuspectedLine::Off => false,
        SuspectedLine::Validated => contract.is_some_and(is_validated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(kind: &str, model: Option<&str>, dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: kind.into(),
            model: model.map(Into::into),
            dim,
        }
    }

    #[test]
    fn only_bge_m3_is_validated() {
        let candle = contract(
            "candle",
            Some("BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181"),
            1024,
        );
        let llama = contract("bge_m3", Some("bge-m3-FP16"), 1024);
        let llama_default = contract("bge_m3", None, 1024);
        let eg2 = contract(
            "embeddinggemma2",
            Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2"),
            768,
        );
        let other_weights = contract("candle", Some("someone/finetune@abc"), 1024);
        let narrow = contract("bge_m3", None, 512);
        let fixture = contract("fixture", None, 1024);
        for c in [&candle, &llama, &llama_default] {
            assert!(is_validated(c), "{c:?}");
        }
        for c in [&eg2, &other_weights, &narrow, &fixture] {
            assert!(!is_validated(c), "{c:?}");
        }
    }

    #[test]
    fn the_mode_decides() {
        let bge = contract("bge_m3", None, 1024);
        let eg2 = contract("embeddinggemma2", None, 768);
        assert!(shows_line(SuspectedLine::Validated, Some(&bge)));
        assert!(!shows_line(SuspectedLine::Validated, Some(&eg2)));
        assert!(!shows_line(SuspectedLine::Validated, None));
        assert!(shows_line(SuspectedLine::On, Some(&eg2)));
        assert!(shows_line(SuspectedLine::On, None));
        assert!(!shows_line(SuspectedLine::Off, Some(&bge)));
        assert_eq!(SuspectedLine::default(), SuspectedLine::Validated);
    }

    #[test]
    fn the_toml_values() {
        #[derive(Deserialize)]
        struct W {
            v: SuspectedLine,
        }
        for (text, want) in [
            ("validated", SuspectedLine::Validated),
            ("on", SuspectedLine::On),
            ("off", SuspectedLine::Off),
        ] {
            let w: W = toml::from_str(&format!("v = \"{text}\"")).unwrap();
            assert_eq!(w.v, want);
            assert_eq!(want.to_string(), text);
        }
        assert!(toml::from_str::<W>("v = \"always\"").is_err());
        // The `[embedder]` key, as `lambo.toml` spells it.
        let e: crate::embed::EmbedderConfig =
            toml::from_str("kind = \"bge_m3\"\nsuspected_supersession_line = \"off\"").unwrap();
        assert_eq!(e.suspected_supersession_line, SuspectedLine::Off);
        let e: crate::embed::EmbedderConfig = toml::from_str("kind = \"bge_m3\"").unwrap();
        assert_eq!(e.suspected_supersession_line, SuspectedLine::Validated);
    }
}
