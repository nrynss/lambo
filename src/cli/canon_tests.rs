//! `lambo canon` against a real SQLite file holding the fair-test fixture
//! and a live lease row (#36 PR 5): the input is unchanged by hash, its lease
//! is untouched, nothing appears beside it, and the scratch copy is removed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::store::lease::LeaseHolder;
use crate::store::{GraphStore, SqliteStore};
use crate::test_util::fair_test as fair;
use crate::test_util::ScratchDir;
use crate::types::AgentId;

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// The fixture in a SQLite file with a live lease on its session, written
/// with a rollback journal so the file is final at each commit, as an
/// online backup is (a WAL-mode builder's pool would checkpoint after this
/// returns, in the background, and race the test's first hash).
async fn fixture_db(dir: &Path) -> (PathBuf, crate::store::LeaseInfo) {
    use std::str::FromStr;
    let path = dir.join("store.db");
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(path.to_str().unwrap())
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
    let store = SqliteStore::new(options);
    store.init_schema().await.unwrap();
    store.seed(&fair::snapshot(true)).await.unwrap();
    let holder = LeaseHolder::for_this_process(&AgentId::from("operator"));
    store
        .acquire_lease(&fair::session(), &holder, Duration::from_secs(3_600))
        .await
        .unwrap();
    let lease = store.read_lease(&fair::session()).await.unwrap().unwrap();
    drop(store);
    (path, lease)
}

fn common(store: PathBuf, scratch: &Path, json: bool) -> CommonArgs {
    CommonArgs {
        store,
        session: fair::NAME.into(),
        policy: PolicyChoice::Both,
        at: Some(fair::now()),
        json,
        quiet: 5,
        max_cycles: 500,
        top: 20,
        assume_survived: false,
        scratch: Some(scratch.to_path_buf()),
    }
}

#[tokio::test]
async fn the_input_is_unchanged_its_lease_untouched_and_the_copy_removed() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, lease) = fixture_db(&input_dir).await;
    let files_before = listing(&input_dir);
    let digest_before = imp::fingerprint(&path).unwrap();

    let out = dry_run(DryRunArgs {
        common: common(path.clone(), &scratch, true),
        baseline: None,
    })
    .await
    .unwrap();
    assert!(out.ok, "{}", out.output);
    let report: crate::canon::dry_run::DryRunReport = serde_json::from_str(&out.output).unwrap();
    let source = report.source.as_ref().unwrap();
    assert_eq!(source.sha256_before, source.sha256_after);
    assert_eq!(source.sha256_before, digest_before.digest);
    assert!(source.lease_present);
    assert_eq!(source.lease_token, Some(lease.token));
    assert_eq!(source.lease_live_at_clock, Some(true));
    assert!(report
        .invariants
        .iter()
        .any(|i| i.name == "source_unchanged" && i.held));
    // Both policies reach the fixture's four Canonicals.
    assert!(report.policies.iter().all(|p| p.canonical_total == 4));

    // Nothing beside the input, no copy left behind, the hash unchanged.
    assert_eq!(listing(&input_dir), files_before);
    assert!(listing(&scratch).is_empty(), "{:?}", listing(&scratch));
    assert_eq!(imp::fingerprint(&path).unwrap(), digest_before);

    // The lease row is the one the writer holds: not taken, renewed or
    // stolen, and its fencing token not minted again.
    let store = SqliteStore::connect(path.to_str().unwrap()).unwrap();
    assert_eq!(
        store.read_lease(&fair::session()).await.unwrap(),
        Some(lease)
    );
    assert!(store
        .load_session(&fair::session())
        .await
        .unwrap()
        .canonization_events
        .is_empty());
}

#[tokio::test]
async fn the_alias_preview_reads_a_map_and_flips_the_alias_on_the_copy() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let map = input_dir.join("aliases.map");
    std::fs::write(
        &map,
        format!(
            "# reviewed\n{} {}\n",
            fair::concept_id("lease_alias").0,
            &fair::concept_id("lease_stays").0.simple().to_string()[..8]
        ),
    )
    .unwrap();
    let files_before = listing(&input_dir);
    let digest_before = imp::fingerprint(&path).unwrap();
    let out = alias_preview(AliasPreviewArgs {
        common: common(path.clone(), &scratch, false),
        map: Some(map),
        verbose: false,
    })
    .await
    .unwrap();
    assert!(out.ok, "{}", out.output);
    let alias = fair::concept_id("lease_alias");
    assert!(
        out.output
            .contains(&format!("Alias {alias} Entity: Venerable -> None")),
        "{}",
        out.output
    );
    assert!(out.output.contains("ok   source_unchanged"));
    assert!(out.output.contains("nothing is applied"));
    assert_eq!(listing(&input_dir), files_before);
    assert!(listing(&scratch).is_empty());
    assert_eq!(imp::fingerprint(&path).unwrap(), digest_before);
}

#[tokio::test]
async fn bad_inputs_are_usage_errors_and_leave_no_copy() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let mut args = common(path.clone(), &scratch, false);
    args.session = "no-such-session".into();
    let err = dry_run(DryRunArgs {
        common: args,
        baseline: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    let err = dry_run(DryRunArgs {
        common: common(input_dir.join("missing.db"), &scratch, false),
        baseline: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    let err = dry_run(DryRunArgs {
        common: common(input_dir.to_path_buf(), &scratch, false),
        baseline: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    let garbage = input_dir.join("baseline.json");
    std::fs::write(&garbage, "{}").unwrap();
    let err = dry_run(DryRunArgs {
        common: common(path, &scratch, false),
        baseline: Some(garbage),
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
    assert!(listing(&scratch).is_empty());
    assert!(PolicyChoice::parse("both").is_ok());
    assert!(PolicyChoice::parse("SOLO").is_ok());
    assert_eq!(PolicyChoice::parse("everyone").unwrap_err().exit_code(), 2);
    assert_eq!(parse_at("yesterday").unwrap_err().exit_code(), 2);
}

/// A backup taken as a WAL-mode file plus its `-wal` sidecar, with the
/// fixture still uncheckpointed in the sidecar: the copy takes both, so the
/// session is read in full, and neither input file changes.
#[tokio::test]
async fn a_store_with_an_uncheckpointed_wal_is_read_in_full() {
    let live_dir = ScratchDir::new("lambo-canon-live");
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let live = live_dir.join("store.db");
    let store = SqliteStore::connect(live.to_str().unwrap()).unwrap();
    store.init_schema().await.unwrap();
    store.seed(&fair::snapshot(true)).await.unwrap();
    // Copy while the writer is still open: the data sits in the -wal.
    let path = input_dir.join("store.db");
    std::fs::copy(&live, &path).unwrap();
    std::fs::copy(
        live_dir.join("store.db-wal"),
        input_dir.join("store.db-wal"),
    )
    .unwrap();
    drop(store);
    let before = imp::fingerprint(&path).unwrap();
    assert!(before.wal);
    let files_before = listing(&input_dir);
    let out = dry_run(DryRunArgs {
        common: common(path.clone(), &scratch, true),
        baseline: None,
    })
    .await
    .unwrap();
    assert!(out.ok, "{}", out.output);
    let report: crate::canon::dry_run::DryRunReport = serde_json::from_str(&out.output).unwrap();
    assert_eq!(report.concepts, fair::CONCEPTS.len());
    assert!(report.policies.iter().all(|p| p.canonical_total == 4));
    assert_eq!(imp::fingerprint(&path).unwrap(), before);
    assert_eq!(listing(&input_dir), files_before);
    assert!(listing(&scratch).is_empty());
}
