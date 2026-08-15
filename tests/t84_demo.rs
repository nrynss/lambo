//! T8.4 — the spec §13 demo scenario, proven deterministic.
//!
//! The bar the task sets is "identical outcomes, every run", not "usually
//! works", so each backend here runs the **whole** scenario twice in one
//! process and asserts the two [`DemoOutcome`]s are equal — the concept and
//! edge counts, every concept's canonization status, the `canonization_events`
//! audit trail in commit order, the canonical set with its blast radius, and
//! agent B's context block byte for byte (with the conflict line's one
//! genuinely time-derived integer normalized; see
//! `cli::demo::normalize_conflict_age`).
//!
//! Each run gets a **fresh session id** on every backend, which is the P6
//! review R3-1 carveout: canonization state is not restored over an existing
//! session on SQLite/Cockroach, so re-running into a used session would
//! silently produce a demo that does not transition. The stores themselves are
//! shared between the two runs, so run 2 really does start from a store that
//! already holds run 1's session — the shape a live ×2 run has.
//!
//! The live-cluster ×2 run cannot happen here; `demo/LIVE-RUNBOOK.md` carries
//! the exact commands and expected output for it.
#![cfg(all(feature = "store-memory", feature = "embed-fixture"))]

use std::sync::Arc;

use lambo::cli::demo::{
    self, Args, DemoOutcome, DemoRun, EXPECT_BLAST_RADIUS, EXPECT_BLAST_WARNING,
    EXPECT_CANONICAL_LABEL, EXPECT_CONCEPTS, EXPECT_CONFLICT_LINE, EXPECT_INTERACTIONS,
    SCENARIO_REST_API, USER_SCHEMA, USER_SCHEMA_DEPENDENTS,
};
use lambo::embed::FixtureEmbedder;
use lambo::types::EmbeddingContract;
use lambo::{Embedder, GraphStore, MemoryStore};

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    }
}

async fn run_once(store: &Arc<dyn GraphStore>, embedder: &Arc<dyn Embedder>) -> DemoRun {
    demo::run_scenario(
        store.clone(),
        embedder.clone(),
        contract(),
        Args {
            scenario: SCENARIO_REST_API.into(),
            // `None` = a fresh session id per run (R3-1).
            session: None,
        },
        false,
    )
    .await
    .expect("demo scenario")
}

/// Everything spec §13 promises, checked against one run's outcome.
fn assert_spec_13(outcome: &DemoOutcome) {
    assert_eq!(outcome.scenario, SCENARIO_REST_API);
    assert_eq!(outcome.interactions, EXPECT_INTERACTIONS);
    assert_eq!(outcome.concepts, EXPECT_CONCEPTS);

    // Step 2 — Candidate -> Venerable -> Canonical, one audit row per hop.
    let hops: Vec<(&str, &str)> = outcome
        .transitions
        .iter()
        .filter(|t| t.content == USER_SCHEMA)
        .map(|t| (t.from.as_str(), t.to.as_str()))
        .collect();
    assert_eq!(
        hops,
        vec![
            ("None", "Candidate"),
            ("Candidate", "Venerable"),
            ("Venerable", "Canonical"),
        ],
        "user schema must climb the spec §10 state machine one legal hop at a \
         time, with a canonization_events row for each: {:?}",
        outcome.transitions
    );

    // The demo must not have produced canonization state any other way.
    assert!(
        outcome
            .statuses
            .iter()
            .any(|(c, s)| c == USER_SCHEMA && s == "Canonical"),
        "{:?}",
        outcome.statuses
    );
    assert_eq!(
        outcome.canonical,
        vec![(USER_SCHEMA.to_string(), EXPECT_BLAST_RADIUS)],
        "user schema is the only concept in the script with a Stage-3 blast \
         radius above the floor"
    );

    // The nine dependents are still nine, and none of them was promoted.
    for dependent in USER_SCHEMA_DEPENDENTS {
        let status = outcome
            .statuses
            .iter()
            .find(|(c, _)| c == dependent)
            .map(|(_, s)| s.as_str());
        assert_eq!(
            status,
            Some("None"),
            "dependent '{dependent}' should never leave None"
        );
    }

    // Step 3 — the context block, asserted on the exact strings.
    let ctx = &outcome.recall_context;
    assert!(
        ctx.contains(EXPECT_CANONICAL_LABEL),
        "context block is missing the canonical marker:\n{ctx}"
    );
    assert!(
        ctx.contains(EXPECT_BLAST_WARNING),
        "context block is missing the spec §13 ⚑ line verbatim:\n{ctx}"
    );
    assert!(
        ctx.contains(EXPECT_CONFLICT_LINE),
        "context block is missing the recency conflict line:\n{ctx}"
    );
    assert!(
        ctx.contains(&format!("blast radius {EXPECT_BLAST_RADIUS}")),
        "the hit line must carry the blast radius:\n{ctx}"
    );
    assert!(
        outcome
            .recall_warnings
            .iter()
            .any(|w| w == EXPECT_BLAST_WARNING),
        "the ⚑ line must also reach the warnings channel: {:?}",
        outcome.recall_warnings
    );
}

/// The transcript is the video's script; it must name each act and quote the
/// promotion log, so a reviewer can read a run without a debugger.
fn assert_transcript(run: &DemoRun) {
    let text = run.transcript.join("\n");
    for beat in [
        "ACT I",
        "ACT II",
        "ACT III",
        "ACT IV",
        "CANONIZATION",
        "OUTCOME",
        "canonization_edge_min_age",
        "→ Candidate   (canonization_events row written)",
        "→ Venerable   (canonization_events row written)",
        "→ Canonical   (canonization_events row written)",
        "the state machine is at its fixed point",
        "nothing in this session is collectable",
        EXPECT_BLAST_WARNING,
        "agent-b does not make the breaking change.",
    ] {
        assert!(
            text.contains(beat),
            "transcript is missing {beat:?}:\n{text}"
        );
    }
    // Every knob the demo compresses has to be on screen, not just in a doc.
    assert!(text.contains("no threshold weakened"), "{text}");
}

#[test]
fn scenario_is_identical_twice_on_the_memory_store() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let embedder: Arc<dyn Embedder> = Arc::new(FixtureEmbedder::new());

    let (first, second) = runtime.block_on(async {
        let first = run_once(&store, &embedder).await;
        let second = run_once(&store, &embedder).await;
        (first, second)
    });

    assert_spec_13(&first.outcome);
    assert_spec_13(&second.outcome);
    assert_transcript(&first);
    assert_eq!(
        first.outcome,
        second.outcome,
        "two runs of the same scenario must produce identical outcomes\n\
         ---- run 1 ----\n{}\n---- run 2 ----\n{}",
        first.outcome.render(),
        second.outcome.render()
    );
    runtime.shutdown_background();
}

#[cfg(feature = "store-sqlite")]
mod sqlite {
    use super::*;
    use lambo::store::SqliteStore;

    fn scratch_db() -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "lambo-t84-demo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let db = dir.join("demo.sqlite");
        let path = db.to_str().expect("utf-8 path").to_string();
        (dir, path)
    }

    /// The same ×2 bar against a durable store: run 2 attaches to a file that
    /// already holds run 1's session, and still has to produce byte-identical
    /// outcomes in its own fresh session (R3-1).
    #[test]
    fn scenario_is_identical_twice_on_sqlite() {
        let (dir, path) = scratch_db();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let (first, second) = runtime.block_on(async {
            let store: Arc<dyn GraphStore> =
                Arc::new(SqliteStore::connect(&path).expect("sqlite connect"));
            let embedder: Arc<dyn Embedder> = Arc::new(FixtureEmbedder::new());
            let first = run_once(&store, &embedder).await;
            let second = run_once(&store, &embedder).await;
            (first, second)
        });

        assert_spec_13(&first.outcome);
        assert_spec_13(&second.outcome);
        assert_eq!(
            first.outcome,
            second.outcome,
            "two runs against one SQLite file must produce identical outcomes\n\
             ---- run 1 ----\n{}\n---- run 2 ----\n{}",
            first.outcome.render(),
            second.outcome.render()
        );

        runtime.shutdown_background();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `--scenario` is validated, and the error names what is valid.
#[test]
fn an_unknown_scenario_is_a_usage_error_that_lists_the_valid_ones() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let embedder: Arc<dyn Embedder> = Arc::new(FixtureEmbedder::new());
    let err = runtime
        .block_on(demo::run_scenario(
            store,
            embedder,
            contract(),
            Args {
                scenario: "graphql-api".into(),
                session: None,
            },
            false,
        ))
        .expect_err("unknown scenario must be refused");
    assert_eq!(err.exit_code(), 2, "usage errors exit 2");
    let msg = err.to_string();
    assert!(msg.contains("graphql-api"), "{msg}");
    assert!(msg.contains(SCENARIO_REST_API), "{msg}");
}
