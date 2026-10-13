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
        supersede: false,
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
        watch: Vec::new(),
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
    // Both policies reach the fixture's six Canonicals.
    assert!(report.policies.iter().all(|p| p.canonical_total == 6));

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
    assert!(report.policies.iter().all(|p| p.canonical_total == 6));
    assert_eq!(imp::fingerprint(&path).unwrap(), before);
    assert_eq!(listing(&input_dir), files_before);
    assert!(listing(&scratch).is_empty());
}

/// Review H2: a reviewed map with any refused line makes the command fail
/// (exit 1 through `Outcome::ok`), with the report still printed.
#[tokio::test]
async fn a_reviewed_map_with_a_refused_line_fails_the_command() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let map = input_dir.join("aliases.map");
    std::fs::write(&map, "writer lease and something\n").unwrap();
    let out = alias_preview(AliasPreviewArgs {
        common: common(path, &scratch, false),
        map: Some(map),
        verbose: false,
    })
    .await
    .unwrap();
    assert!(!out.ok, "{}", out.output);
    assert!(
        out.output.contains("FAIL every_reviewed_line_accepted"),
        "{}",
        out.output
    );
    assert!(
        !out.output.contains("writer lease"),
        "the map line is not echoed"
    );
}

/// Review L1: a stop signal while the work runs removes the scratch copy
/// and reports it; the work task is aborted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_run_removes_its_scratch_copy() {
    let parent = ScratchDir::new("lambo-canon-scratch");
    let scratch = imp::Scratch::new(Some(&parent)).unwrap();
    let dir = scratch.path().to_path_buf();
    std::fs::write(dir.join("store.db"), b"copy").unwrap();
    // The work notes when its future is dropped: an aborted task drops it.
    struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let guard = Dropped(flag.clone());
    let work = async move {
        let _guard = guard;
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        Ok::<(), CliError>(())
    };
    let err = imp::guarded(scratch, work, async { "SIGINT" })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("interrupted by SIGINT"), "{err}");
    // Sonnet review 1: the work task is aborted, not left running.
    for _ in 0..1_000 {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst),
        "the work task was not aborted"
    );
    assert!(!dir.exists());
    assert!(listing(&parent).is_empty());
    // A completed run reports its removal too.
    let scratch = imp::Scratch::new(Some(&parent)).unwrap();
    let ((), removed) = imp::guarded(scratch, async { Ok(()) }, std::future::pending())
        .await
        .unwrap();
    assert_eq!(removed, Ok(()));
    assert!(listing(&parent).is_empty());
}

/// Review L6: the scratch directory is private to the operator.
#[cfg(unix)]
#[test]
fn the_scratch_directory_is_0700() {
    use std::os::unix::fs::PermissionsExt;
    let parent = ScratchDir::new("lambo-canon-scratch");
    let scratch = imp::Scratch::new(Some(&parent)).unwrap();
    let mode = std::fs::metadata(scratch.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    scratch.remove().unwrap();
}

/// Review L6: an input written while it is copied is refused, and nothing
/// is loaded from the copy.
#[tokio::test]
async fn an_input_that_changes_while_it_is_copied_is_refused() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let c = common(path.clone(), &scratch, false);
    let target = path.clone();
    let writer = move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .unwrap();
        f.write_all(b"a live writer").unwrap();
    };
    let err = match imp::load_copy(&c, &scratch, fair::now(), &writer).await {
        Ok(_) => panic!("a changed input must be refused"),
        Err(e) => e,
    };
    assert_eq!(err.exit_code(), 1);
    assert!(
        err.to_string().contains("changed while it was copied"),
        "{err}"
    );
}

/// Review L5: a hot rollback journal is refused (opening would roll it
/// back, a write), and a path holding `?` and `%` is a path.
#[tokio::test]
async fn a_hot_journal_is_refused_and_odd_paths_open() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let odd = input_dir.join("store?mode=memory%20x.db");
    std::fs::copy(&path, &odd).unwrap();
    let out = dry_run(DryRunArgs {
        common: common(odd.clone(), &scratch, true),
        baseline: None,
    })
    .await
    .unwrap();
    assert!(out.ok, "{}", out.output);
    std::fs::write(imp::sidecar(&odd, "-journal"), b"hot").unwrap();
    let err = dry_run(DryRunArgs {
        common: common(odd, &scratch, true),
        baseline: None,
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains("-journal"), "{err}");
    assert!(listing(&scratch).is_empty());
}

/// Review L2: a baseline of another session is refused before the run.
#[tokio::test]
async fn a_baseline_of_another_session_is_refused() {
    let input_dir = ScratchDir::new("lambo-canon-input");
    let scratch = ScratchDir::new("lambo-canon-scratch");
    let (path, _) = fixture_db(&input_dir).await;
    let out = dry_run(DryRunArgs {
        common: common(path.clone(), &scratch, true),
        baseline: None,
    })
    .await
    .unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&out.output).unwrap();
    v["session"] = serde_json::json!("another");
    let base = input_dir.join("base.json");
    std::fs::write(&base, v.to_string()).unwrap();
    let err = dry_run(DryRunArgs {
        common: common(path, &scratch, true),
        baseline: Some(base),
    })
    .await
    .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("session"), "{err}");
}

/// Sonnet review 3: the handlers install synchronously, before any scratch
/// directory exists, and installing them does not resolve the stop future.
#[tokio::test]
async fn the_signal_handlers_install_before_the_scratch_copy() {
    let stop = imp::install_signals().unwrap();
    let raced = tokio::time::timeout(Duration::from_millis(20), stop).await;
    assert!(
        raced.is_err(),
        "no signal arrived, so the stop future must still wait"
    );
}
