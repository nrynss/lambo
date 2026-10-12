//! #33 PR 3: a layered serve's start checks (`layers`), one refusal per
//! rule, and the stdio rule.

use crate::mcp::serve::layers::{check_contracts, check_coverage, check_pinned_layers};
use super::*;
use crate::config::{ServeConfig, ServeCredential, ServeLayers};
use crate::surface::session::{parse_addressed, SessionCapabilities, SessionGrant, SessionScope};
use crate::types::EmbeddingContract;

fn fake(label: &str) -> SecretToken {
    SecretToken::new(["fake", label, "pr3", "value"].join("-")).expect("non-empty")
}

fn credential(name: &str, sessions: &[&str], every_hosted: bool) -> ServeCredential {
    ServeCredential {
        grant: SessionGrant::new(
            name,
            SessionScope::new(
                sessions
                    .iter()
                    .map(|s| parse_addressed(s).expect("addressable")),
                every_hosted,
                None,
            ),
            SessionCapabilities::default(),
        ),
        token: fake(name),
    }
}

fn config(serve: &str) -> ServeConfig {
    crate::LamboFile::from_toml_str(&format!("[serve]\n{serve}"))
        .expect("parse")
        .serve
}

/// `proj` layered with `general`, both pinned.
fn proj_on_general() -> (ServeOptions, ServeLayers) {
    let cfg = config("sessions = [\"proj\", \"general\"]\nlayers = [\"general\"]\n");
    let layers = cfg.resolve_layers(&cfg.sessions).expect("resolve");
    let mut opts = ServeOptions::new("proj", "agent-test");
    opts.sessions = cfg.sessions.clone();
    opts.transport = Transport::Http;
    opts.layers = layers.clone();
    (opts, layers)
}

#[test]
fn a_credential_reaching_a_layered_session_must_reach_its_layer() {
    let (mut opts, layers) = proj_on_general();
    opts.credentials = vec![
        credential("both", &["proj", "general"], false),
        credential("narrow", &["proj"], false),
    ];
    let err = check_coverage(&authority_for(&opts), &layers)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("credential \"narrow\" reaches \"proj\" but not its layer \"general\""),
        "{err}"
    );
    assert!(!err.contains("fake"), "never a token: {err}");

    // A credential reaching only the layer is fine: it is not layered.
    opts.credentials = vec![credential("layer-only", &["general"], false)];
    check_coverage(&authority_for(&opts), &layers).expect("layer only");
    // "*" reaches every pinned session.
    opts.credentials = vec![credential("star", &[], true)];
    check_coverage(&authority_for(&opts), &layers).expect("star");
    // The implicit `local` and the legacy `default` cover every pinned one.
    opts.credentials = Vec::new();
    check_coverage(&authority_for(&opts), &layers).expect("implicit local");
    opts.auth_token = Some(fake("legacy"));
    check_coverage(&authority_for(&opts), &layers).expect("legacy default");
}

#[test]
fn a_stack_must_name_only_pinned_sessions() {
    let (_, layers) = proj_on_general();
    check_pinned_layers(&layers, &["proj".into(), "general".into()]).expect("pinned");
    let err = check_pinned_layers(&layers, &["proj".into(), "other".into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("every layer must be pinned"), "{err}");
    let err = check_pinned_layers(&layers, &["general".into()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("layered but not pinned"), "{err}");
}

#[test]
fn a_layer_must_share_its_consumers_embedding_contract() {
    let (_, layers) = proj_on_general();
    let contract = |model: Option<&str>, dim| EmbeddingContract {
        kind: "fixture".into(),
        model: model.map(str::to_string),
        dim,
    };
    let same = contract(None, 1024);
    check_contracts(&layers, |_| Some(&same)).expect("equal");
    // A session not acquired at start (held elsewhere) is not checked here.
    check_contracts(&layers, |id| (id == "proj").then_some(&same)).expect("held layer");

    let wide = contract(None, 768);
    let err = check_contracts(&layers, |id| Some(if id == "proj" { &same } else { &wide }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("different embedding contracts"), "{err}");

    // The #134 note's L6: unset and empty models differ, and say so.
    let blank = contract(Some(""), 1024);
    let err = check_contracts(&layers, |id| {
        Some(if id == "proj" { &same } else { &blank })
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("unset in one, an empty string in the other"),
        "{err}"
    );
}

/// Stdio holds one session and serves it unlayered: `plan_layers` gives no
/// stacks, and `stdio_layer_warning` names the layers it does not read,
/// only when the session would be layered.
#[test]
fn a_stdio_serve_is_unlayered_and_says_so_once() {
    let cfg = config(
        "sessions = [\"proj\", \"general\"]\nlayers = [\"general\"]\n[serve.session_layers]\nout = []\n",
    );
    assert!(plan_layers(&cfg, &["proj".into()], Transport::Stdio)
        .expect("stdio never refuses")
        .is_empty());
    let warning = stdio_layer_warning(&cfg, "proj", Transport::Stdio).expect("warned");
    assert!(warning.contains("serves it unlayered"), "{warning}");
    assert!(warning.contains("general"), "{warning}");
    assert!(stdio_layer_warning(&cfg, "general", Transport::Stdio).is_none());
    assert!(stdio_layer_warning(&cfg, "out", Transport::Stdio).is_none());
    assert!(stdio_layer_warning(&cfg, "proj", Transport::Http).is_none());
    // Over HTTP the same table resolves, and refuses an unpinned layer.
    let layers = plan_layers(&cfg, &cfg.sessions, Transport::Http).expect("resolve");
    assert_eq!(layers.stack("proj")[0].session, "general");
    let err = plan_layers(&cfg, &["proj".into()], Transport::Http)
        .unwrap_err()
        .to_string();
    assert!(err.contains("every layer must be pinned"), "{err}");
}
