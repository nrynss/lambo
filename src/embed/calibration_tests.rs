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
fn merge_threshold_applies_unless_switched_off() {
    let default = crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT;
    assert_eq!(merge_threshold(true, &eg2(), default), Some(0.93));
    // Switched off: the configured threshold stays (a warning is logged).
    assert_eq!(merge_threshold(false, &eg2(), default), None);
    assert_eq!(merge_threshold(false, &eg2(), 0.99), None);
    let mut unknown = eg2();
    unknown.model = None;
    assert_eq!(merge_threshold(true, &unknown, default), None);
}

/// Owner decision on #203: the calibrated merge threshold is the default.
#[test]
fn calibrated_merge_threshold_is_on_by_default() {
    let cfg = crate::embed::EmbedderConfig::default();
    assert_eq!(cfg.calibration, CalibrationMode::Builtin);
    assert!(cfg.calibrated_merge_threshold);
    let config = crate::Config::default();
    assert_eq!(config.calibration, CalibrationMode::Builtin);
    assert!(config.calibrated_merge_threshold);
}

/// The file's default matches the struct's: absent means on, and `false`
/// is the off switch.
#[test]
fn calibrated_merge_threshold_parses_with_default_on() {
    let parse = |extra: &str| {
        toml::from_str::<crate::embed::EmbedderConfig>(&format!("kind = \"bge_m3\"\n{extra}"))
            .unwrap()
            .calibrated_merge_threshold
    };
    assert!(parse(""));
    assert!(parse("calibrated_merge_threshold = true"));
    assert!(!parse("calibrated_merge_threshold = false"));
    let config: crate::Config =
        serde_json::from_value(serde_json::to_value(crate::Config::default()).unwrap()).unwrap();
    assert!(config.calibrated_merge_threshold);
    let mut value = serde_json::to_value(crate::Config::default()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("calibrated_merge_threshold");
    let config: crate::Config = serde_json::from_value(value).unwrap();
    assert!(
        config.calibrated_merge_threshold,
        "an absent field defaults on"
    );
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

/// #87 follow-up: the unset layer floor is the row's under `builtin` with
/// the switch on, BGE-M3's 0.50 otherwise; a configured floor always wins.
#[test]
fn layer_floor_resolves_from_the_contract_unless_set_or_switched_off() {
    use crate::recall::layered::DEFAULT_LAYER_MIN_SIMILARITY as BGE;
    let row = &BUILTIN[0];
    let eg2 = EmbeddingContract {
        kind: row.kind.into(),
        model: Some(row.model.into()),
        dim: row.dim,
    };
    let other = EmbeddingContract {
        dim: 512,
        ..eg2.clone()
    };
    assert_eq!(
        layer_min_similarity(CalibrationMode::Builtin, true, &eg2),
        0.72
    );
    assert_eq!(
        layer_min_similarity(CalibrationMode::Builtin, false, &eg2),
        BGE
    );
    assert_eq!(layer_min_similarity(CalibrationMode::Off, true, &eg2), BGE);
    assert_eq!(
        layer_min_similarity(CalibrationMode::Builtin, true, &other),
        BGE
    );

    let on = crate::Config::default();
    assert!(on.calibrated_layer_floor);
    assert_eq!(resolve_layer_floor(None, &on, &eg2), 0.72);
    for set in [0.0, 0.5, 0.6, 0.9, 1.0] {
        assert_eq!(resolve_layer_floor(Some(set), &on, &eg2), set);
    }
    let off = crate::Config {
        calibrated_layer_floor: false,
        ..crate::Config::default()
    };
    assert_eq!(resolve_layer_floor(None, &off, &eg2), BGE);
    assert_eq!(resolve_layer_floor(Some(0.6), &off, &eg2), 0.6);
}

/// `[embedder] calibrated_layer_floor` parses, defaulting on, and reaches
/// a `Config` serialised before the field existed as on.
#[test]
fn calibrated_layer_floor_parses_with_default_on() {
    let parse = |extra: &str| -> crate::embed::EmbedderConfig {
        toml::from_str(&format!("kind = \"bge_m3\"\n{extra}")).expect("parse")
    };
    assert!(parse("").calibrated_layer_floor);
    assert!(parse("calibrated_layer_floor = true").calibrated_layer_floor);
    assert!(!parse("calibrated_layer_floor = false").calibrated_layer_floor);
    assert!(crate::embed::EmbedderConfig::default().calibrated_layer_floor);
    let mut json = serde_json::to_value(crate::Config::default()).unwrap();
    json.as_object_mut()
        .unwrap()
        .remove("calibrated_layer_floor");
    let back: crate::Config = serde_json::from_value(json).unwrap();
    assert!(back.calibrated_layer_floor);
}

/// A floor below the calibrated one, set or switched off, is warned of;
/// the calibrated floor, a higher one and a contract without a row are not.
#[test]
fn a_floor_below_the_calibrated_one_is_warned_of() {
    let row = &BUILTIN[0];
    let eg2 = EmbeddingContract {
        kind: row.kind.into(),
        model: Some(row.model.into()),
        dim: row.dim,
    };
    let bge = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    let on = crate::Config::default();
    let off = crate::Config {
        calibrated_layer_floor: false,
        ..crate::Config::default()
    };
    let set = layer_floor_warning("general", Some(0.5), &on, &eg2).expect("0.50 < 0.72");
    assert!(set.contains("min_similarity = 0.5"), "{set}");
    assert!(set.contains("0.72") && set.contains(row.evidence), "{set}");
    let switched = layer_floor_warning("general", None, &off, &eg2).expect("off");
    assert!(
        switched.contains("calibrated_layer_floor = false"),
        "{switched}"
    );
    assert_eq!(layer_floor_warning("general", None, &on, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.72), &on, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.8), &off, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.5), &on, &bge), None);
}
