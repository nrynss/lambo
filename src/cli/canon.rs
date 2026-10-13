//! `lambo canon dry-run` and `lambo canon alias-preview` (#36 PR 5): the
//! operator's read-only look at what canonization would do to a session,
//! before a writer is re-pinned to a build that changes it.
//!
//! Both work on a **private copy** of a SQLite store file:
//!
//! 1. the input file (and its `-wal`, when there is one) is hashed and
//!    copied, byte for byte, into a fresh scratch directory (mode 0700,
//!    under `--scratch` or the system temp directory), and the copy's hash is
//!    checked against the input's;
//! 2. only the copy is opened, read-only, and the session is loaded from it
//!    into memory; the copied lease row is read and reported, never taken,
//!    renewed or stolen;
//! 3. the evaluator runs on an in-memory overlay ([`crate::canon::dry_run`]);
//! 4. the scratch directory is removed, the input is hashed again, and the
//!    report's `source_unchanged` invariant says whether the two hashes agree
//!    and no `-wal`/`-shm` appeared beside the input.
//!
//! The input is never opened by SQLite, so not even a shared-memory file is
//! created beside it. Nothing binds a port. Output is ids, type and status
//! labels and counts; `--json` gives the same report as JSON. Any failed
//! invariant makes the command exit 1 after printing the report.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::caps::CliError;
use crate::canon::PromotionPolicy;

/// Which policies to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyChoice {
    /// One policy.
    One(PromotionPolicy),
    /// Swarm, then Solo.
    Both,
}

impl PolicyChoice {
    /// Parse `swarm`, `solo` or `both` (case-insensitive).
    pub fn parse(s: &str) -> Result<Self, CliError> {
        if s.trim().eq_ignore_ascii_case("both") {
            return Ok(Self::Both);
        }
        s.parse::<PromotionPolicy>()
            .map(Self::One)
            .map_err(|e| CliError::Usage(format!("--policy: {e}, or both")))
    }

    /// The policies to run, in order.
    pub fn policies(self) -> Vec<PromotionPolicy> {
        match self {
            Self::One(p) => vec![p],
            Self::Both => vec![PromotionPolicy::Swarm, PromotionPolicy::Solo],
        }
    }
}

/// The options both subcommands share.
#[derive(Clone, Debug)]
pub struct CommonArgs {
    /// The SQLite store file to copy.
    pub store: PathBuf,
    /// The session.
    pub session: String,
    /// Policies to run.
    pub policy: PolicyChoice,
    /// The evaluation clock (default: now).
    pub at: Option<DateTime<Utc>>,
    /// JSON instead of text.
    pub json: bool,
    /// Floor on consecutive empty cycles.
    pub quiet: usize,
    /// Safety limit on cycles.
    pub max_cycles: usize,
    /// Ids per list.
    pub top: usize,
    /// Raise survival counts to 3 first.
    pub assume_survived: bool,
    /// Where the private copy goes (default: the system temp directory).
    pub scratch: Option<PathBuf>,
}

/// `lambo canon dry-run`'s options.
#[derive(Clone, Debug)]
pub struct DryRunArgs {
    /// Shared options.
    pub common: CommonArgs,
    /// An earlier `--json` report to compare with.
    pub baseline: Option<PathBuf>,
}

/// `lambo canon alias-preview`'s options.
#[derive(Clone, Debug)]
pub struct AliasPreviewArgs {
    /// Shared options.
    pub common: CommonArgs,
    /// A reviewed map (`alias-id ruling-id` per line); detection otherwise.
    pub map: Option<PathBuf>,
    /// Also print both full dry runs per policy.
    pub verbose: bool,
}

/// What a command printed and whether every invariant held.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The report, text or JSON.
    pub output: String,
    /// Whether every invariant held (exit 0) or not (exit 1).
    pub ok: bool,
}

/// Parse `--at`.
pub fn parse_at(s: &str) -> Result<DateTime<Utc>, CliError> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| CliError::Usage(format!("--at: {e} (expected RFC 3339)")))
}

#[cfg(all(feature = "store-sqlite", feature = "store-memory"))]
mod imp {
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use sqlx::sqlite::SqliteConnectOptions;

    use super::{AliasPreviewArgs, CommonArgs, DryRunArgs, Outcome};
    use crate::canon::alias_preview::{alias_preview, render_alias_preview, PairInput};
    use crate::canon::dry_run::{
        compare, dry_run_all, render_report, DryRunParams, DryRunReport, Invariant, SourceFacts,
    };
    use crate::cli::caps::{check_size_cli, require_nonempty, CliError};
    use crate::store::{GraphStore, SqliteStore};
    use crate::types::{GraphSnapshot, SessionId, StoreError};

    /// The input's fingerprint: SHA-256 of the file and, when present, of
    /// its `-wal`; and which sidecars exist.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Fingerprint {
        pub digest: String,
        pub wal: bool,
        pub shm: bool,
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    }

    pub(super) fn fingerprint(path: &Path) -> Result<Fingerprint, CliError> {
        use sha2::{Digest, Sha256};
        let read = |p: &Path| {
            std::fs::read(p).map_err(|e| CliError::Runtime(format!("{}: {e}", p.display())))
        };
        let mut h = Sha256::new();
        h.update(read(path)?);
        let wal_path = sidecar(path, "-wal");
        let wal = wal_path.exists();
        if wal {
            h.update(b"\0wal\0");
            h.update(read(&wal_path)?);
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(Fingerprint {
            digest,
            wal,
            shm: sidecar(path, "-shm").exists(),
        })
    }

    /// A private scratch directory, removed on drop.
    pub(super) struct Scratch(PathBuf);

    impl Scratch {
        pub(super) fn new(parent: Option<&Path>) -> Result<Self, CliError> {
            let parent = parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
            let dir = parent.join(format!(
                "lambo-canon-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            let mut b = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(&dir)
                .map_err(|e| CliError::Runtime(format!("scratch {}: {e}", dir.display())))?;
            Ok(Self(dir))
        }

        pub(super) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The loaded copy and what the CLI learned about the source.
    struct Loaded {
        snapshot: GraphSnapshot,
        facts: SourceFacts,
        before: Fingerprint,
    }

    async fn load_copy(
        c: &CommonArgs,
        scratch: &Scratch,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Loaded, CliError> {
        require_nonempty("session", &c.session)?;
        check_size_cli("session", &c.session)?;
        let meta = std::fs::metadata(&c.store)
            .map_err(|e| CliError::Usage(format!("--store {}: {e}", c.store.display())))?;
        if !meta.is_file() {
            return Err(CliError::Usage(format!(
                "--store {}: not a file (a SQLite store file is expected)",
                c.store.display()
            )));
        }
        let before = fingerprint(&c.store)?;
        let copy = scratch.path().join("store.db");
        std::fs::copy(&c.store, &copy)
            .map_err(|e| CliError::Runtime(format!("copy {}: {e}", c.store.display())))?;
        if before.wal {
            std::fs::copy(sidecar(&c.store, "-wal"), sidecar(&copy, "-wal"))
                .map_err(|e| CliError::Runtime(format!("copy the -wal: {e}")))?;
        }
        if fingerprint(&copy)?.digest != before.digest {
            return Err(CliError::Runtime(format!(
                "{} changed while it was copied: is a live writer using it? \
                 Take a backup and run on that.",
                c.store.display()
            )));
        }
        let path = copy
            .to_str()
            .ok_or_else(|| CliError::Runtime("scratch path is not UTF-8".into()))?;
        let options = SqliteConnectOptions::from_str(path)
            .map_err(|e| CliError::Runtime(format!("open the copy: {e}")))?
            .read_only(true);
        let store = SqliteStore::new(options);
        let session = SessionId::new(&c.session);
        let snapshot = match store.load_session(&session).await {
            Ok(s) => s,
            Err(StoreError::SessionNotFound(_)) => {
                return Err(CliError::Usage(format!(
                    "session {:?} is not in {}",
                    c.session,
                    c.store.display()
                )))
            }
            Err(e) => return Err(CliError::Runtime(e.to_string())),
        };
        if snapshot.concepts.is_empty() {
            return Err(CliError::Usage(format!(
                "session {:?} has no concepts in {}",
                c.session,
                c.store.display()
            )));
        }
        let lease = store
            .read_lease(&session)
            .await
            .map_err(|e| CliError::Runtime(e.to_string()))?;
        drop(store);
        let facts = SourceFacts {
            path: c.store.display().to_string(),
            sha256_before: before.digest.clone(),
            sha256_after: String::new(),
            lease_present: lease.is_some(),
            lease_token: lease.as_ref().map(|l| l.token),
            lease_live_at_clock: lease.as_ref().map(|l| l.expires_at > now),
            unconsumed_intents: snapshot
                .write_intents
                .iter()
                .filter(|i| i.outcome.is_none())
                .count(),
        };
        Ok(Loaded {
            snapshot,
            facts,
            before,
        })
    }

    fn base_params(c: &CommonArgs, now: chrono::DateTime<chrono::Utc>) -> DryRunParams {
        let mut p = DryRunParams::new(crate::canon::PromotionPolicy::Swarm, now);
        p.min_quiet_cycles = c.quiet;
        p.max_cycles = c.max_cycles;
        p.top = c.top;
        p.assume_survived = c.assume_survived;
        p
    }

    /// Hash the input again and say whether it is unchanged.
    fn source_check(c: &CommonArgs, before: &Fingerprint, facts: &mut SourceFacts) -> Invariant {
        match fingerprint(&c.store) {
            Ok(after) => {
                facts.sha256_after = after.digest.clone();
                Invariant::new(
                    "source_unchanged",
                    after == *before,
                    if after == *before {
                        String::new()
                    } else {
                        format!(
                            "sha256 {} -> {}, -wal {} -> {}, -shm {} -> {}",
                            before.digest,
                            after.digest,
                            before.wal,
                            after.wal,
                            before.shm,
                            after.shm
                        )
                    },
                )
            }
            Err(e) => Invariant::new("source_unchanged", false, e.to_string()),
        }
    }

    pub(super) async fn dry_run(args: DryRunArgs) -> Result<Outcome, CliError> {
        let c = &args.common;
        let baseline: Option<DryRunReport> = match &args.baseline {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| CliError::Usage(format!("--baseline {}: {e}", p.display())))?;
                Some(serde_json::from_str(&text).map_err(|e| {
                    CliError::Usage(format!(
                        "--baseline {}: not a `canon dry-run --json` report: {e}",
                        p.display()
                    ))
                })?)
            }
            None => None,
        };
        let now = c.at.unwrap_or_else(chrono::Utc::now);
        let scratch = Scratch::new(c.scratch.as_deref())?;
        let Loaded {
            snapshot,
            mut facts,
            before,
        } = load_copy(c, &scratch, now).await?;
        let mut report = dry_run_all(snapshot, &c.policy.policies(), &base_params(c, now))
            .await
            .map_err(|e| CliError::Runtime(e.to_string()))?;
        drop(scratch);
        let check = source_check(c, &before, &mut facts);
        report.invariants.push(check);
        report.source = Some(facts);
        report.comparison = baseline.as_ref().map(|b| compare(b, &report));
        let ok = report.invariants.iter().all(|i| i.held)
            && report
                .policies
                .iter()
                .all(|p| p.invariants.iter().all(|i| i.held));
        let output = if c.json {
            serde_json::to_string_pretty(&report).map_err(|e| CliError::Runtime(e.to_string()))?
        } else {
            render_report(&report)
        };
        Ok(Outcome { output, ok })
    }

    pub(super) async fn alias_preview_run(args: AliasPreviewArgs) -> Result<Outcome, CliError> {
        let c = &args.common;
        let input = match &args.map {
            Some(p) => PairInput::Map(
                std::fs::read_to_string(p)
                    .map_err(|e| CliError::Usage(format!("--map {}: {e}", p.display())))?,
            ),
            None => PairInput::Detect,
        };
        let now = c.at.unwrap_or_else(chrono::Utc::now);
        let scratch = Scratch::new(c.scratch.as_deref())?;
        let Loaded {
            snapshot,
            mut facts,
            before,
        } = load_copy(c, &scratch, now).await?;
        let mut report =
            alias_preview(snapshot, &input, &c.policy.policies(), &base_params(c, now))
                .await
                .map_err(|e| CliError::Runtime(e.to_string()))?;
        drop(scratch);
        let check = source_check(c, &before, &mut facts);
        report.invariants.push(check);
        report.source = Some(facts);
        let ok = report.invariants.iter().all(|i| i.held);
        let output = if c.json {
            serde_json::to_string_pretty(&report).map_err(|e| CliError::Runtime(e.to_string()))?
        } else {
            render_alias_preview(&report, args.verbose)
        };
        Ok(Outcome { output, ok })
    }
}

/// `lambo canon dry-run`.
pub async fn dry_run(args: DryRunArgs) -> Result<Outcome, CliError> {
    #[cfg(all(feature = "store-sqlite", feature = "store-memory"))]
    {
        imp::dry_run(args).await
    }
    #[cfg(not(all(feature = "store-sqlite", feature = "store-memory")))]
    {
        let _ = args;
        Err(unavailable())
    }
}

/// `lambo canon alias-preview`.
pub async fn alias_preview(args: AliasPreviewArgs) -> Result<Outcome, CliError> {
    #[cfg(all(feature = "store-sqlite", feature = "store-memory"))]
    {
        imp::alias_preview_run(args).await
    }
    #[cfg(not(all(feature = "store-sqlite", feature = "store-memory")))]
    {
        let _ = args;
        Err(unavailable())
    }
}

#[cfg(not(all(feature = "store-sqlite", feature = "store-memory")))]
fn unavailable() -> CliError {
    CliError::Usage(
        "lambo canon needs a build with the store-sqlite and store-memory features \
         (released binaries carry both)"
            .into(),
    )
}

/// The scratch parent a test or an operator passes, checked to exist.
pub fn check_scratch(p: &Path) -> Result<PathBuf, CliError> {
    if p.is_dir() {
        Ok(p.to_path_buf())
    } else {
        Err(CliError::Usage(format!(
            "--scratch {}: not a directory",
            p.display()
        )))
    }
}

#[cfg(all(test, feature = "store-sqlite", feature = "fixtures"))]
#[path = "canon_tests.rs"]
mod tests;
