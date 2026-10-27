use super::*;

#[test]
fn the_default_is_max_so_existing_users_rank_as_before() {
    let f = RecallFusion::default();
    assert_eq!(f.kind, FusionKind::Max);
    assert_eq!(f.k, DEFAULT_RRF_K);
    assert_eq!(f.keyword_weight, DEFAULT_LEG_WEIGHT);
    assert_eq!(f.vector_weight, DEFAULT_LEG_WEIGHT);
    assert_eq!(f.recent_weight, DEFAULT_RECENT_WEIGHT);
    f.validate().expect("the default validates");
}

#[test]
fn an_empty_table_parses_to_the_default_and_a_partial_one_keeps_the_rest() {
    let empty: RecallFusion = toml::from_str("").expect("empty table");
    assert_eq!(empty, RecallFusion::default());
    let rrf: RecallFusion = toml::from_str("kind = \"rrf\"\nk = 20.0").expect("partial table");
    assert_eq!(rrf.kind, FusionKind::Rrf);
    assert_eq!(rrf.k, 20.0);
    assert_eq!(rrf.vector_weight, DEFAULT_LEG_WEIGHT);
    assert_eq!(rrf.recent_weight, DEFAULT_RECENT_WEIGHT);
}

#[test]
fn an_unknown_key_or_kind_is_refused() {
    assert!(toml::from_str::<RecallFusion>("kindd = \"rrf\"").is_err());
    assert!(toml::from_str::<RecallFusion>("kind = \"RRF\"").is_err());
    assert!(toml::from_str::<RecallFusion>("kind = \"sum\"").is_err());
}

#[test]
fn display_matches_the_config_spelling() {
    assert_eq!(FusionKind::Max.to_string(), "max");
    assert_eq!(FusionKind::Rrf.to_string(), "rrf");
}

#[test]
fn validate_refuses_a_negative_or_non_finite_value() {
    for bad in [-0.1, f64::NAN, f64::INFINITY] {
        for set in [
            (|f: &mut RecallFusion, v| f.k = v) as fn(&mut RecallFusion, f64),
            |f, v| f.keyword_weight = v,
            |f, v| f.vector_weight = v,
            |f, v| f.recent_weight = v,
        ] {
            let mut f = RecallFusion::default();
            set(&mut f, bad);
            let err = f.validate().expect_err("refused").to_string();
            assert!(err.contains("[fusion]"), "{err}");
        }
    }
}

#[test]
fn validate_refuses_rrf_with_every_weight_zero_but_not_max() {
    let zero = RecallFusion {
        keyword_weight: 0.0,
        vector_weight: 0.0,
        recent_weight: 0.0,
        ..RecallFusion::default()
    };
    zero.validate().expect("max ignores the weights");
    let err = RecallFusion {
        kind: FusionKind::Rrf,
        ..zero
    }
    .validate()
    .expect_err("rrf with nothing to rank")
    .to_string();
    assert!(err.contains("at least one leg weight"), "{err}");
}

#[test]
fn k_zero_is_allowed_and_rank_one_is_the_weight() {
    let f = RecallFusion {
        k: 0.0,
        ..RecallFusion::default()
    };
    f.validate().expect("k = 0 is valid: ranks start at 1");
    assert_eq!(f.term(1.0, 1), 1.0);
    assert_eq!(f.term(1.0, 4), 0.25);
}

#[test]
fn a_term_falls_with_rank() {
    let f = RecallFusion::default();
    assert!(f.term(1.0, 1) > f.term(1.0, 2));
    assert_eq!(f.term(1.0, 1), 1.0 / 61.0);
    assert_eq!(f.term(0.0, 1), 0.0);
}
