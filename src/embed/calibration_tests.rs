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

/// Only the exact measured Qwen3 Embedding 4B contracts have rows: a
/// neighbouring width, an unnamed model, or the other width's row do not
/// transfer.
#[test]
fn only_the_exact_measured_qwen_contracts_have_rows() {
    for dim in [2560, 1024] {
        let exact = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim,
        };
        assert!(lookup(&exact).is_some(), "dim {dim}");
    }
    for c in [
        EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim: 512,
        },
        EmbeddingContract {
            kind: "bge_m3".into(),
            model: None,
            dim: 2560,
        },
        EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim: 2048,
        },
        EmbeddingContract {
            kind: "openai".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim: 2560,
        },
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
/// the same contract, so the table and the evidence directories cannot drift
/// apart. Each row names its own evidence file (`evidence = ...`), and each
/// evidence file holds that row's contracts.
#[test]
fn rows_match_the_committed_evidence() {
    let files = [
        (
            include_str!("../../evidence/issue-87-calibration/calibration.json"),
            "evidence/issue-87-calibration",
        ),
        (
            include_str!("../../evidence/issue-221/calibration-qwen3-4b-2560.json"),
            "evidence/issue-221",
        ),
        (
            include_str!("../../evidence/issue-221/calibration-qwen3-4b-1024.json"),
            "evidence/issue-221",
        ),
        (
            include_str!("../../evidence/issue-237/calibration-elastic-jina-v3-1024.json"),
            "evidence/issue-237",
        ),
    ];
    let mut checked = 0;
    for (text, dir) in files {
        let evidence: serde_json::Value = serde_json::from_str(text).unwrap();
        for e in evidence["contracts"].as_array().unwrap() {
            let row = BUILTIN
                .iter()
                .find(|row| row.kind == e["kind"] && row.model == e["model"] && row.dim == e["dim"])
                .unwrap_or_else(|| panic!("no built-in row for evidence contract {e}"));
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
            assert!(row.evidence.starts_with(dir), "{row:?}");
            checked += 1;
        }
    }
    assert_eq!(checked, BUILTIN.len(), "every built-in row is evidenced");
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

/// #221: the BGE-M3 reference contracts run on calibrated constants (they
/// ARE the scale), everything else needs a row. The Vultr Qwen3 Embedding 4B
/// contracts — `kind = "bge_m3"` (alias `"openai"`) with a Qwen model, at
/// 2560 or at the 1024 truncation — have measured rows (evidence/issue-221/),
/// so they are calibrated without being reference.
#[test]
fn calibrated_means_a_row_or_the_bge_m3_reference() {
    let reference = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    assert!(is_reference(&reference));
    assert!(is_calibrated(&reference));
    // An explicit BGE-M3 model name is still the reference space.
    for model in ["bge-m3", "bge_m3", "bge", "BAAI/bge-m3", "@cf/baai/bge-m3"] {
        let named = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some(model.into()),
            dim: 1024,
        };
        assert!(is_reference(&named), "{model}");
        assert!(is_calibrated(&named), "{model}");
    }
    // The candle adapter's canonical BGE-M3 artifact is the #87 reference run.
    let candle = EmbeddingContract {
        kind: "candle".into(),
        model: Some("BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181 model.safetensors sha256:68440cc1b73b".into()),
        dim: 1024,
    };
    assert!(is_reference(&candle));
    assert!(is_calibrated(&candle));
    // The measured EG2 contract has a row.
    assert!(is_calibrated(&eg2()));
    // #221: the Qwen3 Embedding 4B contracts at either width have measured
    // rows now (evidence/issue-221/).
    for (dim, recent, floor, merge) in [(2560, 0.41, 0.59, 0.89), (1024, 0.42, 0.60, 0.90)] {
        let qwen = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim,
        };
        assert!(!is_reference(&qwen), "dim {dim}");
        assert!(is_calibrated(&qwen), "dim {dim}");
        let row = lookup(&qwen).unwrap_or_else(|| panic!("dim {dim}"));
        assert_eq!(row.recent_score, recent, "dim {dim}");
        assert_eq!(row.layer_min_similarity, floor, "dim {dim}");
        assert_eq!(row.merge_threshold, merge, "dim {dim}");
    }
    // A custom candle repo and a non-BGE model at BGE dims are not reference.
    let custom_repo = EmbeddingContract {
        kind: "candle".into(),
        model: Some("dir:<local> custom.safetensors sha256:abc".into()),
        dim: 1024,
    };
    assert!(!is_calibrated(&custom_repo));
    let other_dim = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 512,
    };
    assert!(!is_calibrated(&other_dim));
}

/// P1-2 (review round 1): `is_reference` is anchored, not a substring — a
/// finetune-style name that merely mentions BGE-M3 is a different measured
/// space until it is measured, so it warns instead of silently borrowing
/// BGE-M3's constants. Quant/file variants of the artifact (`bge-m3-FP16`,
/// `BGE_M3.Q8_0.gguf`) are likewise not the reference run (candle f32 on
/// the Hugging Face weights).
#[test]
fn finetune_style_names_are_not_reference() {
    for model in [
        "my-bge-m3-ft",
        "bge-m3-finetune",
        "my-bge_m3-custom",
        "xbge-m3",
        "bge-m3x",
        "not-bge-m3",
        "bge-m3-FP16",
        "BGE_M3.Q8_0.gguf",
        "bge-m3-old.gguf",
    ] {
        let named = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some(model.into()),
            dim: 1024,
        };
        assert!(!is_reference(&named), "{model}");
        assert!(!is_calibrated(&named), "{model}");
        assert!(lookup(&named).is_none(), "{model}");
    }
    // Case is still folded: the anchored ids match in any case.
    for model in ["BGE-M3", "BAAI/BGE-M3", "@CF/BAAI/BGE-M3"] {
        let named = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some(model.into()),
            dim: 1024,
        };
        assert!(is_reference(&named), "{model}");
    }
}

/// #221 ask 1: the Qwen3 Embedding 4B rows are measured (see
/// evidence/issue-221/README.md for the exact commands). This test pins the
/// measured state: it fails if either row leaves `lookup`, so the rows and
/// their evidence files land together, as `rows_match_the_committed_evidence`
/// requires.
#[test]
fn qwen3_embedding_4b_rows_match_their_evidence() {
    for (dim, recent, floor, merge, merges, file) in [
        (
            2560,
            0.41,
            0.59,
            0.89,
            0.70,
            include_str!("../../evidence/issue-221/calibration-qwen3-4b-2560.json"),
        ),
        (
            1024,
            0.42,
            0.60,
            0.90,
            0.71,
            include_str!("../../evidence/issue-221/calibration-qwen3-4b-1024.json"),
        ),
    ] {
        let contract = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim,
        };
        let row = lookup(&contract).unwrap_or_else(|| {
            panic!(
                "qwen3-embedding-4b at dim {dim}: no measured row — measure it \
                 like #203 on the #87 set (evidence/issue-221/README.md) and \
                 ship the row with its evidence, instead of guessing constants"
            )
        });
        assert_eq!(row.evidence, "evidence/issue-221/", "dim {dim}");
        let evidence: serde_json::Value = serde_json::from_str(file).unwrap();
        let e = &evidence["contracts"][0];
        assert_eq!(e["kind"], "bge_m3", "dim {dim}");
        assert_eq!(e["model"], "qwen3-embedding-4b", "dim {dim}");
        assert_eq!(e["dim"], dim, "dim {dim}");
        assert_eq!(e["recent_score"]["shipped"], recent, "dim {dim}");
        assert_eq!(e["layer_min_similarity"]["shipped"], floor, "dim {dim}");
        assert_eq!(e["merge_threshold"]["shipped"], merge, "dim {dim}");
        assert_eq!(row.recent_score, recent, "dim {dim}");
        assert_eq!(row.layer_min_similarity, floor, "dim {dim}");
        assert_eq!(row.merge_threshold, merge, "dim {dim}");
        assert_eq!(row.default_threshold_merges, merges, "dim {dim}");
    }
}

/// #221 follow-up (fusion): the unset-`[fusion]` default is RRF exactly for
/// the Qwen3 Embedding 4B contracts under `builtin` calibration — the space
/// whose narrow cosine band makes max-merge keyword-only — and `max`
/// everywhere else. `calibration = "off"` keeps `max` even on Qwen (legacy
/// behavior exactly); an explicit `[fusion]` table is applied by resolve and
/// never reaches this default.
#[test]
fn unset_fusion_defaults_to_rrf_only_for_qwen_under_builtin() {
    use crate::recall::fusion::{FusionKind, RecallFusion};
    let max = RecallFusion::default();
    assert_eq!(max.kind, FusionKind::Max);
    for dim in [2560, 1024] {
        let qwen = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim,
        };
        let scoped = default_fusion(CalibrationMode::Builtin, &qwen);
        assert_eq!(scoped.kind, FusionKind::Rrf, "dim {dim}");
        // #173's defaults, not new numbers: k and the leg weights ship
        // unchanged from the measured opt-in.
        assert_eq!(scoped.k, crate::recall::fusion::DEFAULT_RRF_K, "dim {dim}");
        assert_eq!(
            scoped.keyword_weight,
            crate::recall::fusion::DEFAULT_LEG_WEIGHT,
            "dim {dim}"
        );
        assert_eq!(
            scoped.vector_weight,
            crate::recall::fusion::DEFAULT_LEG_WEIGHT,
            "dim {dim}"
        );
        assert_eq!(
            scoped.recent_weight,
            crate::recall::fusion::DEFAULT_RECENT_WEIGHT,
            "dim {dim}"
        );
        scoped.validate().expect("the scoped default validates");
        assert_eq!(
            default_fusion(CalibrationMode::Off, &qwen),
            max,
            "calibration off keeps max, dim {dim}"
        );
    }
    // A neighbouring width is a different space: it keeps max.
    let narrow = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("qwen3-embedding-4b".into()),
        dim: 512,
    };
    assert_eq!(
        default_fusion(CalibrationMode::Builtin, &narrow),
        max,
        "unmeasured width keeps max"
    );
    // BGE-M3 (the reference) and EmbeddingGemma 2 (its own row) keep max.
    for c in [
        EmbeddingContract {
            kind: "bge_m3".into(),
            model: None,
            dim: 1024,
        },
        eg2(),
    ] {
        assert_eq!(
            default_fusion(CalibrationMode::Builtin, &c),
            max,
            "{c:?} keeps max"
        );
    }
}
/// #221 ask 2: the uncalibrated warning fires exactly when it should — a
/// contract with no row that is not the reference, under `builtin` — and
/// stays silent for a row, the reference, and `calibration = "off"`. The
/// Qwen3 Embedding 4B contracts have rows now, so a foreign model stands in
/// for the uncalibrated shape.
#[test]
fn warn_if_uncalibrated_fires_once_per_process_shape() {
    let foreign = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("some-unmeasured-model".into()),
        dim: 2048,
    };
    assert!(lookup(&foreign).is_none());
    assert!(warn_if_uncalibrated(CalibrationMode::Builtin, &foreign));
    assert!(!warn_if_uncalibrated(CalibrationMode::Off, &foreign));
    // Qwen3 Embedding 4B at either measured width warns no more.
    for dim in [2560, 1024] {
        let qwen = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("qwen3-embedding-4b".into()),
            dim,
        };
        assert!(!warn_if_uncalibrated(CalibrationMode::Builtin, &qwen));
    }
    assert!(!warn_if_uncalibrated(CalibrationMode::Builtin, &eg2()));
    let reference = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    assert!(!warn_if_uncalibrated(CalibrationMode::Builtin, &reference));
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
        switched.contains("[embedder] calibrated_layer_floor = false"),
        "{switched}"
    );
    assert!(
        switched.contains("Turn the calibration back on"),
        "{switched}"
    );
    let mode_off = crate::Config {
        calibration: CalibrationMode::Off,
        ..crate::Config::default()
    };
    let by_mode = layer_floor_warning("general", None, &mode_off, &eg2).expect("mode off");
    assert!(
        by_mode.contains("[embedder] calibration = \"off\""),
        "{by_mode}"
    );
    // Set and switched off: removing the line alone would still leave 0.50.
    let both = layer_floor_warning("general", Some(0.5), &off, &eg2).expect("both");
    assert!(
        both.contains("Remove that line and turn the calibration back on"),
        "{both}"
    );
    assert!(
        set.starts_with("[[serve.layer]]") && set.contains("Remove that line to"),
        "{set}"
    );
    assert_eq!(layer_floor_warning("general", None, &on, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.72), &on, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.8), &off, &eg2), None);
    assert_eq!(layer_floor_warning("general", Some(0.5), &on, &bge), None);
}

/// #233: an `elastic` contract is never the BGE-M3 reference, even when
/// the inference endpoint id names BGE-M3 exactly or carries the
/// `;input_type=off` suffix: an endpoint id is an operator-chosen name, not
/// a measured artifact. #237: the exact `.jina-embeddings-v3` contract at
/// 1024 dims has a measured row (evidence/issue-237/), so it is calibrated
/// without being reference; every neighbouring contract still warns.
#[test]
fn elastic_exact_jina_contract_is_calibrated_but_never_reference() {
    let exact = EmbeddingContract {
        kind: "elastic".into(),
        model: Some(".jina-embeddings-v3".into()),
        dim: 1024,
    };
    assert!(!is_reference(&exact));
    let row = lookup(&exact).expect("exact elastic Jina contract has a row");
    assert_eq!(row.recent_score, 0.13);
    assert_eq!(row.layer_min_similarity, 0.39);
    assert_eq!(row.merge_threshold, 0.86);
    assert_eq!(row.default_threshold_merges, 0.64);
    assert_eq!(row.evidence, "evidence/issue-237/");
    assert!(is_calibrated(&exact));
    assert!(!warn_if_uncalibrated(CalibrationMode::Builtin, &exact));
    assert!(!warn_if_uncalibrated(CalibrationMode::Off, &exact));
    // The row agrees with the committed evidence.
    let evidence: serde_json::Value = serde_json::from_str(include_str!(
        "../../evidence/issue-237/calibration-elastic-jina-v3-1024.json"
    ))
    .unwrap();
    let e = &evidence["contracts"][0];
    assert_eq!(e["kind"], "elastic");
    assert_eq!(e["model"], ".jina-embeddings-v3");
    assert_eq!(e["dim"], 1024);
    assert_eq!(e["recent_score"]["shipped"], 0.13);
    assert_eq!(e["layer_min_similarity"]["shipped"], 0.39);
    assert_eq!(e["merge_threshold"]["shipped"], 0.86);
}

/// #233/#237: neighbouring elastic contracts — `input_type=off`, another
/// width, another endpoint id, or no id — are different spaces with no row,
/// so they stay uncalibrated and warn.
#[test]
fn elastic_neighbouring_contracts_stay_uncalibrated() {
    for model in [
        Some(".jina-embeddings-v3;input_type=off"),
        Some("bge-m3"),
        Some("BAAI/bge-m3"),
        Some("bge-m3;input_type=off"),
        Some(".elser-2-elastic"),
        None,
    ] {
        let contract = EmbeddingContract {
            kind: "elastic".into(),
            model: model.map(str::to_string),
            dim: 1024,
        };
        assert!(!is_reference(&contract), "{model:?}");
        assert!(lookup(&contract).is_none(), "{model:?}");
        assert!(!is_calibrated(&contract), "{model:?}");
        assert!(
            warn_if_uncalibrated(CalibrationMode::Builtin, &contract),
            "{model:?}"
        );
        assert!(!warn_if_uncalibrated(CalibrationMode::Off, &contract));
    }
    // Another width of the exact endpoint id is a different space too.
    let narrow = EmbeddingContract {
        kind: "elastic".into(),
        model: Some(".jina-embeddings-v3".into()),
        dim: 512,
    };
    assert!(!is_reference(&narrow));
    assert!(lookup(&narrow).is_none());
    assert!(!is_calibrated(&narrow));
    assert!(warn_if_uncalibrated(CalibrationMode::Builtin, &narrow));
}
