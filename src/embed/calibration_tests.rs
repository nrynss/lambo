//! #87: the built-in calibration table, its lookup and its evidence.

use super::*;
use crate::recall::candidates::RECENT_SCORE;

fn eg2() -> EmbeddingContract {
    EmbeddingContract {
        kind: "embeddinggemma2".into(),
        model: Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2".into()),
        dim: 768,
    }
}

#[test]
fn only_the_exact_measured_contract_has_a_row() {
    assert_eq!(lookup(&eg2()).map(|r| r.recent_score), Some(0.63));
    let mut other_dim = eg2();
    other_dim.dim = 512;
    let mut other_prompts = eg2();
    other_prompts.model =
        Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v1".into());
    let mut other_quant = eg2();
    other_quant.model =
        Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/F16;prompts=lambo-eg2-v2".into());
    let mut no_model = eg2();
    no_model.model = None;
    let mut other_kind = eg2();
    other_kind.kind = "bge_m3".into();
    let bge = EmbeddingContract {
        kind: "candle".into(),
        model: Some("BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181".into()),
        dim: 1024,
    };
    for c in [
        other_dim,
        other_prompts,
        other_quant,
        no_model,
        other_kind,
        bge,
    ] {
        assert!(lookup(&c).is_none(), "{c:?}");
    }
}

#[test]
fn recent_score_is_calibrated_only_when_on_and_known() {
    assert_eq!(recent_score(CalibrationMode::Builtin, Some(&eg2())), 0.63);
    assert_eq!(
        recent_score(CalibrationMode::Off, Some(&eg2())),
        RECENT_SCORE
    );
    assert_eq!(recent_score(CalibrationMode::Builtin, None), RECENT_SCORE);
    let mut unknown = eg2();
    unknown.dim = 256;
    assert_eq!(
        recent_score(CalibrationMode::Builtin, Some(&unknown)),
        RECENT_SCORE
    );
}

#[test]
fn merge_threshold_applies_only_on_opt_in() {
    let default = crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT;
    assert_eq!(merge_threshold(true, &eg2(), default), Some(0.93));
    // Off by default: the configured threshold stays (a warning is logged).
    assert_eq!(merge_threshold(false, &eg2(), default), None);
    assert_eq!(merge_threshold(false, &eg2(), 0.99), None);
    let mut unknown = eg2();
    unknown.model = None;
    assert_eq!(merge_threshold(true, &unknown, default), None);
}

#[test]
fn defaults_change_nothing_for_merges() {
    let cfg = crate::embed::EmbedderConfig::default();
    assert_eq!(cfg.calibration, CalibrationMode::Builtin);
    assert!(!cfg.calibrated_merge_threshold);
    let config = crate::Config::default();
    assert_eq!(config.calibration, CalibrationMode::Builtin);
    assert!(!config.calibrated_merge_threshold);
}

#[test]
fn modes_parse_from_toml() {
    #[derive(serde::Deserialize)]
    struct Wrap {
        calibration: CalibrationMode,
    }
    let parse = |s: &str| toml::from_str::<Wrap>(s).map(|w| w.calibration);
    assert_eq!(
        parse("calibration = \"builtin\"").unwrap(),
        CalibrationMode::Builtin
    );
    assert_eq!(
        parse("calibration = \"off\"").unwrap(),
        CalibrationMode::Off
    );
    assert!(parse("calibration = \"on\"").is_err());
    assert_eq!(CalibrationMode::Off.to_string(), "off");
}

/// Every shipped value is the evidence's derived value at two decimals, for
/// the same contract, so the table and `evidence/issue-87-calibration/`
/// cannot drift apart.
#[test]
fn rows_match_the_committed_evidence() {
    let evidence: serde_json::Value = serde_json::from_str(include_str!(
        "../../evidence/issue-87-calibration/calibration.json"
    ))
    .unwrap();
    let contracts = evidence["contracts"].as_array().unwrap();
    assert_eq!(contracts.len(), BUILTIN.len());
    for row in BUILTIN {
        let e = contracts
            .iter()
            .find(|e| e["kind"] == row.kind && e["model"] == row.model && e["dim"] == row.dim)
            .unwrap_or_else(|| panic!("no evidence for {row:?}"));
        for (name, value) in [
            ("recent_score", row.recent_score),
            ("layer_min_similarity", row.layer_min_similarity),
            ("merge_threshold", row.merge_threshold),
        ] {
            let shipped = e[name]["shipped"].as_f64().unwrap();
            let derived = e[name]["derived"].as_f64().unwrap();
            assert_eq!(shipped, value, "{name}");
            assert!(
                (derived - value).abs() <= 0.005,
                "{name}: {derived} vs {value}"
            );
        }
        let merges = e["default_threshold_merges"].as_f64().unwrap();
        assert!((merges - row.default_threshold_merges).abs() <= 0.005);
        assert!(row.evidence.starts_with("evidence/issue-87-calibration"));
    }
}

#[test]
fn rows_are_ordered_cosines() {
    for row in BUILTIN {
        for v in [
            row.recent_score,
            row.layer_min_similarity,
            row.merge_threshold,
        ] {
            assert!(v.is_finite() && (0.0..=1.0).contains(&v), "{row:?}");
        }
        // The recent leg sits below the layer floor, which sits below a merge.
        assert!(row.recent_score < row.layer_min_similarity, "{row:?}");
        assert!(row.layer_min_similarity < row.merge_threshold, "{row:?}");
    }
}
