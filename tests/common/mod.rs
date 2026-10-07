//! Shared by the integration tests that spawn `lambo serve` (#15).
//!
//! A serve derives its local endpoint under `$XDG_RUNTIME_DIR/lambo`, falling
//! back to `/tmp/lambo-<euid>` when the variable is unset — which is every macOS
//! developer box by default, and is also the directory the operator's live
//! writer uses. A test serve that inherits the ambient environment therefore
//! binds beside the production socket, and one that is SIGKILLed (or whose test
//! aborts) never reaches the clean-exit unlink, so its socket stays there.
//!
//! **Isolation, not cleanup, is what protects production.** Every serve a test
//! spawns gets `XDG_RUNTIME_DIR` pointed at a [`RuntimeDir`] the test owns, so a
//! killed child can only ever leave its socket inside the test's own directory.
//! The guard removes that directory on drop as a courtesy; nothing depends on
//! the drop running.
//!
//! The directory lives directly under `/tmp`, not under `std::env::temp_dir()`:
//! macOS's per-user `TMPDIR` is a 46-byte `/var/folders/...` path, and with
//! `/lambo/` plus the up-to-38-byte socket filename it would crowd the 104-byte
//! `sun_path` bound (see `SESSION_PREFIX_CHARS` in `src/mcp/endpoint.rs`). A
//! base too long for a socket would silently turn the serve's endpoint off and
//! change what the test exercises. `/tmp/lbrt-<pid>-<n>` keeps the full socket
//! path near 70 bytes.
//!
//! Every serve in one test shares one `RuntimeDir`: a holder and the proxy that
//! must find it derive their endpoint from the same base, exactly as two clients
//! on one login session do.

// Each integration test file is its own crate and uses a different subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The variable `endpoint_dir()` consults first.
pub const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";

/// A private (0700), self-owned, per-test runtime directory, removed on drop.
pub struct RuntimeDir {
    path: PathBuf,
}

impl RuntimeDir {
    /// Create a fresh directory. Unique per test process and per call, so tests
    /// running in parallel threads of one binary never share one.
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/lbrt-{}-{n}", std::process::id()));
        // A recycled pid from an earlier aborted run can have left one behind;
        // it is ours by construction of the name, and must start empty.
        let _ = std::fs::remove_dir_all(&path);
        create_private(&path);
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory a serve given this runtime dir derives its endpoint in:
    /// `$XDG_RUNTIME_DIR/lambo`, the first rung of `endpoint_dir_from` in
    /// `src/mcp/endpoint.rs` (private there, so mirrored here). Creates
    /// nothing; for a test asserting where the serve does or does not look.
    pub fn derived_endpoint_dir(&self) -> PathBuf {
        self.path.join("lambo")
    }

    /// [`RuntimeDir::derived_endpoint_dir`], created private as `bind` would
    /// create it. For a test that stands in for a holder the spawned serve must
    /// find.
    pub fn endpoint_dir(&self) -> PathBuf {
        let dir = self.derived_endpoint_dir();
        if !dir.exists() {
            create_private(&dir);
        }
        dir
    }

    /// A private (0700) subdirectory `name` of this runtime dir, created fresh.
    /// Removed with the runtime dir on drop, so a test that binds a socket in it
    /// and then panics leaves nothing outside its own directory. `name` must not
    /// be `lambo`, which is where the spawned serve derives its endpoint.
    pub fn private_subdir(&self, name: &str) -> PathBuf {
        assert_ne!(name, "lambo", "that is the serve's own endpoint directory");
        let dir = self.path.join(name);
        create_private(&dir);
        dir
    }

    /// Point `cmd`'s `XDG_RUNTIME_DIR` at this directory.
    pub fn isolate<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        cmd.env(RUNTIME_DIR_VAR, &self.path)
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn create_private(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .unwrap_or_else(|e| panic!("per-test runtime dir {}: {e}", path.display()));
}

#[cfg(not(unix))]
fn create_private(path: &Path) {
    std::fs::create_dir(path)
        .unwrap_or_else(|e| panic!("per-test runtime dir {}: {e}", path.display()));
}
