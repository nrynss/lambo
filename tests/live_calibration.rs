//! Live BGE-M3 calibration probe (gated; requires a running llama.cpp server).
//!
//! Measures the real cosine distribution of *should-merge* vs *must-not-merge*
//! concept-name pairs so T7.2 can pick a precision-preserving
//! `semantic_match_threshold` for BGE-M3 instead of assuming the
//! FixtureEmbedder-derived 0.85.
//!
//! Run:
//!   ./scripts/run-llama-embed.sh            # start server if not up
//!   cargo test --test live_calibration -- --ignored --nocapture

use lambo::embed::{cosine, BgeM3LlamaCppEmbedder};
use lambo::Embedder;

/// Paraphrases that SHOULD merge (same concept, different surface).
const NEAR_PAIRS: &[(&str, &str)] = &[
    ("register user", "create account"),
    ("user schema", "user data model"),
    ("delete user", "remove account"),
    ("reset password", "change password"),
    ("charge card", "process payment"),
    ("deploy service", "ship application"),
    ("grant access", "authorize user"),
    ("sync data", "reconcile records"),
];

/// Distinct concepts that MUST NOT merge (the false-positive danger zone).
const FAR_PAIRS: &[(&str, &str)] = &[
    ("user schema", "user auth"),
    ("delete user", "create user"),
    ("reset password", "forgot password"),
    ("charge card", "credit score"),
    ("deploy service", "take down service"),
    ("grant access", "revoke access"),
    ("sync data", "log data"),
    ("ship application", "compile binary"),
];

#[tokio::test]
#[ignore]
async fn report_bge3_cosine_distribution() {
    let url = std::env::var("LAMBO_LLAMA_EMBED_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let e = BgeM3LlamaCppEmbedder::new(url, "", 1024).unwrap();
    e.check_health()
        .await
        .expect("llama.cpp server must be running (./scripts/run-llama-embed.sh)");

    let mut near_scores = Vec::new();
    let mut far_scores = Vec::new();

    println!("== NEAR (should merge) ==");
    for (a, b) in NEAR_PAIRS {
        let (va, vb) = tokio::join!(e.embed(a), e.embed(b));
        let s = cosine(&va.unwrap(), &vb.unwrap());
        near_scores.push((s, *a, *b));
        println!("  {s:.3}  {a:24} <-> {b}");
    }
    println!("== FAR (must not merge) ==");
    for (a, b) in FAR_PAIRS {
        let (va, vb) = tokio::join!(e.embed(a), e.embed(b));
        let s = cosine(&va.unwrap(), &vb.unwrap());
        far_scores.push((s, *a, *b));
        println!("  {s:.3}  {a:24} <-> {b}");
    }

    near_scores.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());
    far_scores.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());

    let (min_near, _, _) = near_scores[0];
    let (max_near, _, _) = *near_scores.last().unwrap();
    let (min_far, _, _) = far_scores[0];
    let (max_far, _, _) = *far_scores.last().unwrap();

    println!(
        "\nsummary: BGE-M3 near range [{min_near:.3}, {max_near:.3}], far range [{min_far:.3}, {max_far:.3}]"
    );
    // A threshold strictly between the worst should-merge and the best must-not-merge
    // classifies this corpus perfectly. Report the midpoint as a starting point.
    if min_near > max_far {
        let mid = (min_near + max_far) / 2.0;
        println!("clean gap: any threshold in ({max_far:.3}, {min_near:.3}) separates perfectly");
        println!("suggested starting point: threshold ≈ {mid:.3}");
    } else {
        println!(
            "WARNING: distributions overlap ({max_far:.3} >= {min_near:.3}) - no single threshold separates this corpus"
        );
    }
    println!(
        "for reference: fixture threshold was 0.85; far-max {max_far:.3} / near-min {min_near:.3} show the real BGE-M3 scale."
    );
}

/// Does embedding the concept *with sentence context* separate the same pairs better
/// than embedding the bare 2-3 word name? (Short dense vectors are noisy; context is
/// the standard remedy.)
#[tokio::test]
#[ignore]
async fn context_embedding_separation() {
    let url = std::env::var("LAMBO_LLAMA_EMBED_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let e = BgeM3LlamaCppEmbedder::new(url, "", 1024).unwrap();
    e.check_health().await.unwrap();

    // Same concepts as above, but inside a short sentence rather than a bare label.
    let near: &[(&str, &str)] = &[
        (
            "the service registers a user in the system",
            "the service creates an account in the system",
        ),
        (
            "we deploy the service to production",
            "we ship the application to production",
        ),
        (
            "the operator grants access to the user",
            "the operator authorizes the user",
        ),
    ];
    let far: &[(&str, &str)] = &[
        (
            "reset the user's password now",
            "the user forgot their password earlier",
        ),
        ("delete the user account", "create the user account"),
        ("grant access to the user", "revoke access from the user"),
    ];

    println!("== WITH-CONTEXT: should merge ==");
    for (a, b) in near {
        let (va, vb) = tokio::join!(e.embed(a), e.embed(b));
        println!(
            "  {:.3}  {a:.40} <-> {b:.40}",
            cosine(&va.unwrap(), &vb.unwrap())
        );
    }
    println!("== WITH-CONTEXT: must NOT merge ==");
    for (a, b) in far {
        let (va, vb) = tokio::join!(e.embed(a), e.embed(b));
        println!(
            "  {:.3}  {a:.40} <-> {b:.40}",
            cosine(&va.unwrap(), &vb.unwrap())
        );
    }
}
