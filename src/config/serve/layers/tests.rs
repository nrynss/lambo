//! #33 PR 3: `[serve] layers`, `[serve.session_layers]` and
//! `[[serve.layer]]`, one refusal per rule.

use super::*;
use crate::LamboFile;

fn parse(serve: &str) -> Result<LamboFile, LamboError> {
    LamboFile::from_toml_str(&format!("[store]\nkind = \"memory\"\n\n[serve]\n{serve}"))
}

fn refused(serve: &str) -> String {
    match parse(serve) {
        Ok(f) => panic!("must be refused, parsed as {:?}", f.serve),
        Err(e) => e.to_string(),
    }
}

fn pinned(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_design_example_parses_and_resolves_with_defaults_and_knobs() {
    let f = parse(
        r#"sessions = ["lambo", "rustydocs", "paper", "general"]
layers = ["general"]

[serve.session_layers]
paper = []

[[serve.layer]]
session = "general"
min_similarity = 0.55
"#,
    )
    .expect("parse");
    assert_eq!(f.serve.layers, vec!["general".to_string()]);
    let layers = f.serve.resolve_layers(&f.serve.sessions).expect("resolve");
    let stack = layers.stack("lambo");
    assert_eq!(stack.len(), 1);
    assert_eq!(stack[0].session, "general");
    assert_eq!(stack[0].weight, DEFAULT_LAYER_WEIGHT);
    assert_eq!(stack[0].min_similarity, 0.55);
    assert_eq!(layers.stack("rustydocs"), stack);
    assert!(layers.stack("paper").is_empty(), "opted out");
    assert!(layers.stack("general").is_empty(), "a layer is unlayered");
    assert_eq!(
        layers.iter().map(|(s, _)| s).collect::<Vec<_>>(),
        vec!["lambo", "rustydocs"]
    );
}

#[test]
fn no_layer_keys_resolve_to_no_layers() {
    let f = parse("sessions = [\"a\", \"b\"]\n").expect("parse");
    assert!(f
        .serve
        .resolve_layers(&f.serve.sessions)
        .unwrap()
        .is_empty());
    assert!(f.serve.layer_stack_of("a").is_empty());
}

#[test]
fn every_layer_shape_is_a_list_never_a_scalar() {
    // The parse error never quotes the source line, so it names the shape.
    let err = refused("sessions = [\"a\", \"general\"]\nlayers = \"general\"\n");
    assert!(err.contains("expected a sequence"), "{err}");
    let err = refused("sessions = [\"a\", \"general\"]\n[serve.session_layers]\na = \"general\"\n");
    assert!(err.contains("expected a sequence"), "{err}");
    // `[serve.layer]` as one table instead of an array of tables.
    let err = refused("sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[serve.layer]\nsession = \"general\"\n");
    assert!(err.contains("expected a sequence"), "{err}");
}

#[test]
fn unknown_keys_in_the_layer_tables_stay_a_hard_error() {
    let err = refused(
        "sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[[serve.layer]]\nsession = \"general\"\nwieght = 0.5\n",
    );
    assert!(err.contains("wieght"), "{err}");
    let err = refused("sessions = [\"a\", \"general\"]\nlayer_weight = 0.5\n");
    assert!(err.contains("layer_weight"), "{err}");
}

/// The v1 limit (design 1.1): two layers are refused, naming the limit, in
/// both list forms.
#[test]
fn two_layers_are_refused_naming_the_limit() {
    assert_eq!(MAX_LAYERS, 1);
    let err =
        refused("sessions = [\"a\", \"team\", \"general\"]\nlayers = [\"team\", \"general\"]\n");
    assert!(
        err.contains("lists 2 layers; at most 1 is supported"),
        "{err}"
    );
    let err = refused(
        "sessions = [\"a\", \"team\", \"general\"]\n[serve.session_layers]\na = [\"team\", \"general\"]\n",
    );
    assert!(
        err.contains("\"a\" lists 2 layers; at most 1 is supported"),
        "{err}"
    );
}

#[test]
fn a_session_cannot_be_its_own_layer() {
    let err = refused("sessions = [\"a\", \"general\"]\n[serve.session_layers]\na = [\"a\"]\n");
    assert!(err.contains("cannot be its own layer"), "{err}");
}

#[test]
fn a_layer_cannot_have_layers_of_its_own() {
    let err = refused(
        "sessions = [\"a\", \"team\", \"general\"]\nlayers = [\"team\"]\n[serve.session_layers]\nteam = [\"general\"]\n",
    );
    assert!(err.contains("no transitive layering"), "{err}");
    // Also through two overrides.
    let err = refused(
        "sessions = [\"a\", \"team\", \"general\"]\n[serve.session_layers]\na = [\"team\"]\nteam = [\"general\"]\n",
    );
    assert!(err.contains("no transitive layering"), "{err}");
    // An empty override on a layer is fine: it gives it nothing.
    parse("sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[serve.session_layers]\ngeneral = []\n")
        .expect("an empty override is not a stack");
}

#[test]
fn names_and_duplicates_are_checked() {
    let err = refused("sessions = [\"a\", \"general\"]\nlayers = [\"gen eral\"]\n");
    assert!(err.contains("layers entry"), "{err}");
    let err = refused("sessions = [\"a\", \"general\"]\n[serve.session_layers]\n\"a b\" = []\n");
    assert!(err.contains("session_layers] key"), "{err}");
    let err = refused(
        "sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[[serve.layer]]\nsession = \"general\"\n[[serve.layer]]\nsession = \"general\"\n",
    );
    assert!(err.contains("twice"), "{err}");
}

#[test]
fn a_knob_must_name_a_configured_layer_and_be_in_range() {
    let err = refused(
        "sessions = [\"a\", \"general\"]\n[[serve.layer]]\nsession = \"general\"\nweight = 0.5\n",
    );
    assert!(err.contains("no layers list names"), "{err}");
    for (key, value) in [
        ("weight", "0.0"),
        ("weight", "-1.0"),
        ("weight", "nan"),
        ("weight", "inf"),
        ("min_similarity", "1.5"),
        ("min_similarity", "-0.1"),
        ("min_similarity", "nan"),
    ] {
        let err = refused(&format!(
            "sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[[serve.layer]]\nsession = \"general\"\n{key} = {value}\n"
        ));
        assert!(err.contains(key), "{key} = {value}: {err}");
    }
}

#[test]
fn every_layer_and_layered_session_must_be_pinned_at_resolve() {
    let f = parse("sessions = [\"a\"]\nlayers = [\"general\"]\n").expect("parse");
    let err = f
        .serve
        .resolve_layers(&pinned(&["a"]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("every layer must be pinned"), "{err}");
    // Pinned by `--session` instead of the file is pinned.
    let layers = f
        .serve
        .resolve_layers(&pinned(&["a", "general"]))
        .expect("pinned on the command line");
    assert_eq!(layers.stack("a")[0].session, "general");

    let f = parse("sessions = [\"general\"]\n[serve.session_layers]\napp = [\"general\"]\n")
        .expect("parse");
    let err = f
        .serve
        .resolve_layers(&pinned(&["general"]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("only pinned sessions are layered"), "{err}");
}

#[test]
fn layer_stack_of_reads_the_override_then_the_default() {
    let f = parse(
        "sessions = [\"a\", \"b\", \"general\"]\nlayers = [\"general\"]\n[serve.session_layers]\nb = []\n",
    )
    .expect("parse");
    assert_eq!(f.serve.layer_stack_of("a"), ["general".to_string()]);
    assert!(f.serve.layer_stack_of("b").is_empty());
    assert!(f.serve.layer_stack_of("general").is_empty());
    // Unpinned names still read the default: stdio warns from this.
    assert_eq!(f.serve.layer_stack_of("elsewhere"), ["general".to_string()]);
}

#[test]
fn the_table_round_trips_through_serde() {
    let f = parse(
        "sessions = [\"a\", \"general\"]\nlayers = [\"general\"]\n[serve.session_layers]\na = [\"general\"]\n[[serve.layer]]\nsession = \"general\"\nweight = 0.25\n",
    )
    .expect("parse");
    let text = toml::to_string(&f.serve).expect("serialize");
    let back: ServeConfig = toml::from_str(&text).expect("deserialize");
    assert_eq!(back, f.serve);
}
