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
    /// Concepts to report by id whatever their state.
    pub watch: Vec<String>,
    /// Run with `canonization_supersede` on (#20): supersession and the
    /// Stage 3 hold, as a writer with the setting runs them.
    pub supersede: bool,
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
    /// `--similar` (#178): near-duplicate suggestions instead of detection.
    pub similar: Option<SimilarArgs>,
    /// Also print both full dry runs per policy.
    pub verbose: bool,
}

/// `lambo canon alias-preview --similar`'s options.
#[derive(Clone, Debug)]
pub struct SimilarArgs {
    /// Exact 3-gram Jaccard floor.
    pub jaccard: f64,
    /// Folded characters a concept needs for the lexical source.
    pub min_chars: usize,
    /// Cosine floor for vector pairs; `None` for text only.
    pub cosine: Option<f64>,
    /// Also suggest pairs with a Canonical concept.
    pub include_canonical: bool,
    /// An overrides file (`accept`/`reject` lines).
    pub overrides: Option<PathBuf>,
    /// At most this many suggestions.
    pub max_pairs: usize,
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
    use std::future::Future;
    use std::path::{Path, PathBuf};

    use sqlx::sqlite::SqliteConnectOptions;

    use super::{AliasPreviewArgs, CommonArgs, DryRunArgs, Outcome};
    use crate::canon::alias_preview::{
        alias_preview, render_alias_preview, PairInput, SimilarInput,
    };
    use crate::canon::similar::SimilarParams;
    use crate::canon::dry_run::{
        baseline_mismatch, compare, dry_run_all, redact, render_report, resolve_watch,
        DryRunParams, DryRunReport, Invariant, SourceFacts,
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
        pub journal: bool,
    }

    pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    }

    /// Hash the file and its `-wal`, streamed (a store can be large; review
    /// L5).
    pub(super) fn fingerprint(path: &Path) -> Result<Fingerprint, CliError> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let mut feed = |p: &Path| {
            let mut f = std::fs::File::open(p)
                .map_err(|e| CliError::Runtime(format!("{}: {e}", p.display())))?;
            std::io::copy(&mut f, &mut h)
                .map(|_| ())
                .map_err(|e| CliError::Runtime(format!("{}: {e}", p.display())))
        };
        feed(path)?;
        let wal_path = sidecar(path, "-wal");
        let wal = wal_path.exists();
        if wal {
            feed(&wal_path)?;
        }
        let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok(Fingerprint {
            digest,
            wal,
            shm: sidecar(path, "-shm").exists(),
            journal: sidecar(path, "-journal").exists(),
        })
    }

    /// A private scratch directory. [`Scratch::remove`] removes it and says
    /// whether that worked; dropping it unremoved removes it too, and says
    /// so on stderr when that fails (review L1).
    pub(super) struct Scratch(Option<PathBuf>);

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
            Ok(Self(Some(dir)))
        }

        pub(super) fn path(&self) -> &Path {
            self.0.as_deref().expect("a live scratch directory")
        }

        /// Remove the directory; the error names it.
        pub(super) fn remove(mut self) -> Result<(), String> {
            let dir = self.0.take().expect("a live scratch directory");
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("the scratch copy {} was not removed: {e}", dir.display()))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(dir) = self.0.take()
                && let Err(e) = std::fs::remove_dir_all(&dir)
            {
                eprintln!(
                    "lambo canon: the scratch copy {} was not removed: {e}",
                    dir.display()
                );
            }
        }
    }

    /// The stop future: SIGINT or SIGTERM, resolving to the name of the one
    /// that arrived.
    pub(super) type Stop = std::pin::Pin<Box<dyn Future<Output = &'static str> + Send>>;

    /// Install the SIGINT and SIGTERM handlers **now**, synchronously, and
    /// return the future that waits on them. Callers install them before
    /// the scratch directory exists, so there is no window in which a copy
    /// exists and a signal would kill the process without removing it
    /// (Sonnet review 3): tokio's unix `signal()` registers on call, where
    /// an `async fn` would register only when first polled.
    ///
    /// Off unix, `tokio::signal::ctrl_c()` registers on first poll, which is
    /// when `guarded` starts waiting, just after the scratch directory is
    /// created: a Ctrl-C in that window ends the process with the default
    /// handler and can leave the copy behind (residual, documented).
    pub(super) fn install_signals() -> Result<Stop, CliError> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let install = |kind: SignalKind, name: &str| {
                signal(kind)
                    .map_err(|e| CliError::Runtime(format!("install the {name} handler: {e}")))
            };
            let mut int = install(SignalKind::interrupt(), "SIGINT")?;
            let mut term = install(SignalKind::terminate(), "SIGTERM")?;
            Ok(Box::pin(async move {
                tokio::select! {
                    _ = int.recv() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }))
        }
        #[cfg(not(unix))]
        {
            Ok(Box::pin(async {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }))
        }
    }

    /// Run `work` on its own task beside `stop` (review L1). The work runs
    /// on a worker thread, so a signal is seen even while it computes; when
    /// `stop` wins, the scratch copy is removed before the error returns.
    /// Either way the removal's outcome is returned, never ignored.
    pub(super) async fn guarded<T: Send + 'static>(
        scratch: Scratch,
        work: impl Future<Output = Result<T, CliError>> + Send + 'static,
        stop: impl Future<Output = &'static str>,
    ) -> Result<(T, Result<(), String>), CliError> {
        let mut task = tokio::spawn(work);
        tokio::select! {
            joined = &mut task => {
                let removed = scratch.remove();
                match joined {
                    Ok(Ok(t)) => Ok((t, removed)),
                    Ok(Err(e)) => Err(with_removal(e, removed)),
                    Err(e) => Err(with_removal(CliError::Runtime(format!("the run failed: {e}")), removed)),
                }
            }
            sig = stop => {
                // Stop the work before its scratch copy goes (Sonnet
                // review 1): an abandoned task would keep reading it.
                task.abort();
                let removed = scratch.remove();
                Err(with_removal(CliError::Runtime(format!("interrupted by {sig}")), removed))
            }
        }
    }

    fn with_removal(e: CliError, removed: Result<(), String>) -> CliError {
        match removed {
            Ok(()) => e,
            Err(why) => match e {
                CliError::Usage(m) => CliError::Usage(format!("{m}; {why}")),
                CliError::Runtime(m) => CliError::Runtime(format!("{m}; {why}")),
            },
        }
    }

    fn removal_invariant(removed: Result<(), String>) -> Invariant {
        match removed {
            Ok(()) => Invariant::new("scratch_removed", true, String::new()),
            Err(why) => Invariant::new("scratch_removed", false, why),
        }
    }

    /// The loaded copy and what the CLI learned about the source.
    pub(super) struct Loaded {
        pub snapshot: GraphSnapshot,
        pub facts: SourceFacts,
        pub before: Fingerprint,
    }

    /// Copy the input into `dir`, check nothing changed while it was
    /// copied, and load the session from the copy, read-only. `after_copy`
    /// runs between the copy and the check (a test hook).
    pub(super) async fn load_copy(
        c: &CommonArgs,
        dir: &Path,
        now: chrono::DateTime<chrono::Utc>,
        after_copy: &(dyn Fn() + Send + Sync),
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
        // Review L5: a rollback journal beside the file means a write was
        // in progress or interrupted; the file alone may be torn, and
        // SQLite would roll it back on open, which is a write.
        if before.journal {
            return Err(CliError::Runtime(format!(
                "{} has a -journal beside it: a write is in progress or was interrupted. \
                 Take a backup (sqlite3 .backup) and run on that.",
                c.store.display()
            )));
        }
        let copy = dir.join("store.db");
        std::fs::copy(&c.store, &copy)
            .map_err(|e| CliError::Runtime(format!("copy {}: {e}", c.store.display())))?;
        if before.wal {
            std::fs::copy(sidecar(&c.store, "-wal"), sidecar(&copy, "-wal"))
                .map_err(|e| CliError::Runtime(format!("copy the -wal: {e}")))?;
        }
        after_copy();
        if fingerprint(&copy)?.digest != before.digest || fingerprint(&c.store)? != before {
            return Err(CliError::Runtime(format!(
                "{} changed while it was copied: is a live writer using it? \
                 Take a backup and run on that.",
                c.store.display()
            )));
        }
        // `filename`, not a URL: a path holding `?` or `%` is a path (L5).
        let options = SqliteConnectOptions::new().filename(&copy).read_only(true);
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
            Err(e) => return Err(CliError::Runtime(redact_store(&e))),
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
            .map_err(|e| CliError::Runtime(redact_store(&e)))?;
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

    /// A store error, with an invariant message's text withheld (L3).
    fn redact_store(e: &StoreError) -> String {
        match e {
            StoreError::Invariant(m) => format!("store invariant: {}", redact(m)),
            other => other.to_string(),
        }
    }

    fn base_params(
        c: &CommonArgs,
        now: chrono::DateTime<chrono::Utc>,
        snapshot: &GraphSnapshot,
    ) -> Result<DryRunParams, CliError> {
        let mut p = DryRunParams::new(crate::canon::PromotionPolicy::Swarm, now);
        p.min_quiet_cycles = c.quiet;
        p.max_cycles = c.max_cycles;
        p.top = c.top;
        p.assume_survived = c.assume_survived;
        p.eval.supersede = c.supersede;
        p.watch = resolve_watch(snapshot, &c.watch).map_err(CliError::Usage)?;
        Ok(p)
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
                            "input changed during the run (a live writer?): sha256 {} -> {}, \
                             -wal {} -> {}, -shm {} -> {}, -journal {} -> {}",
                            before.digest,
                            after.digest,
                            before.wal,
                            after.wal,
                            before.shm,
                            after.shm,
                            before.journal,
                            after.journal
                        )
                    },
                )
            }
            Err(e) => Invariant::new(
                "source_unchanged",
                false,
                format!("input changed during the run (a live writer?): {e}"),
            ),
        }
    }

    fn read_baseline(p: &Path) -> Result<DryRunReport, CliError> {
        let text = std::fs::read_to_string(p)
            .map_err(|e| CliError::Usage(format!("--baseline {}: {e}", p.display())))?;
        serde_json::from_str(&text).map_err(|e| {
            CliError::Usage(format!(
                "--baseline {}: not a `canon dry-run --json` report of this version: {e}",
                p.display()
            ))
        })
    }

    pub(super) async fn dry_run(args: DryRunArgs) -> Result<Outcome, CliError> {
        let baseline = args.baseline.as_deref().map(read_baseline).transpose()?;
        // Refuse a baseline of another session or report version before the
        // run, not after it (review L2).
        if let Some(b) = &baseline {
            if b.version != crate::canon::dry_run::REPORT_VERSION {
                return Err(CliError::Usage(format!(
                    "--baseline: the baseline is report version {}, this build writes {}; \
                     re-run the baseline with this build",
                    b.version,
                    crate::canon::dry_run::REPORT_VERSION
                )));
            }
            if b.session.as_str() != args.common.session {
                return Err(CliError::Usage(format!(
                    "--baseline: the baseline is of session {:?}, not {:?}",
                    b.session.as_str(),
                    args.common.session
                )));
            }
        }
        let stop = install_signals()?;
        let scratch = Scratch::new(args.common.scratch.as_deref())?;
        let dir = scratch.path().to_path_buf();
        let c = args.common.clone();
        let work = async move {
            let now = c.at.unwrap_or_else(chrono::Utc::now);
            let loaded = load_copy(&c, &dir, now, &|| {}).await?;
            let params = base_params(&c, now, &loaded.snapshot)?;
            let report = dry_run_all(loaded.snapshot, &c.policy.policies(), &params)
                .await
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            Ok((report, loaded.facts, loaded.before))
        };
        let ((mut report, mut facts, before), removed) = guarded(scratch, work, stop).await?;
        let c = &args.common;
        report.invariants.push(removal_invariant(removed));
        report.invariants.push(source_check(c, &before, &mut facts));
        report.source = Some(facts);
        if let Some(b) = &baseline {
            if let Some(why) = baseline_mismatch(b, &report) {
                return Err(CliError::Usage(format!("--baseline: {why}")));
            }
            report.comparison = Some(compare(b, &report));
        }
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

    /// Validate `--similar`'s options and read the overrides file.
    fn similar_input(s: &super::SimilarArgs) -> Result<SimilarInput, CliError> {
        let unit = |name: &str, v: f64| {
            if v.is_finite() && (0.0..=1.0).contains(&v) {
                Ok(v)
            } else {
                Err(CliError::Usage(format!("--{name} must be in [0, 1], got {v}")))
            }
        };
        let overrides = match &s.overrides {
            Some(p) => std::fs::read_to_string(p)
                .map_err(|e| CliError::Usage(format!("--overrides {}: {e}", p.display())))?,
            None => String::new(),
        };
        crate::canon::similar::split_overrides(&overrides)
            .map_err(|e| CliError::Usage(format!("--overrides: {e}")))?;
        Ok(SimilarInput {
            params: SimilarParams {
                jaccard: unit("jaccard", s.jaccard)?,
                min_chars: s.min_chars,
                cosine: s.cosine.map(|c| unit("cosine", c)).transpose()?,
                include_canonical: s.include_canonical,
                max_pairs: s.max_pairs,
                ..SimilarParams::default()
            },
            overrides,
        })
    }

    pub(super) async fn alias_preview_run(args: AliasPreviewArgs) -> Result<Outcome, CliError> {
        let input = match (&args.map, &args.similar) {
            (Some(_), Some(_)) => {
                return Err(CliError::Usage("--map and --similar are exclusive".into()));
            }
            (Some(p), None) => PairInput::Map(
                std::fs::read_to_string(p)
                    .map_err(|e| CliError::Usage(format!("--map {}: {e}", p.display())))?,
            ),
            (None, Some(s)) => PairInput::Similar(Box::new(similar_input(s)?)),
            (None, None) => PairInput::Detect,
        };
        let stop = install_signals()?;
        let scratch = Scratch::new(args.common.scratch.as_deref())?;
        let dir = scratch.path().to_path_buf();
        let c = args.common.clone();
        let work = async move {
            let now = c.at.unwrap_or_else(chrono::Utc::now);
            let loaded = load_copy(&c, &dir, now, &|| {}).await?;
            let params = base_params(&c, now, &loaded.snapshot)?;
            let report = alias_preview(loaded.snapshot, &input, &c.policy.policies(), &params)
                .await
                .map_err(|e| CliError::Runtime(e.to_string()))?;
            Ok((report, loaded.facts, loaded.before))
        };
        let ((mut report, mut facts, before), removed) = guarded(scratch, work, stop).await?;
        let c = &args.common;
        report.invariants.push(removal_invariant(removed));
        report.invariants.push(source_check(c, &before, &mut facts));
        report.source = Some(facts);
        // Includes `every_reviewed_line_accepted` for a map (review H2) and
        // `both_runs_hold_their_invariants` (M2).
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
