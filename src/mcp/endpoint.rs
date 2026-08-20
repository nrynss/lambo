//! The session endpoint (J2) — where a `lambo serve` holder can be reached.
//!
//! ## Why a serve is reachable at all
//!
//! Spec §2.2 is one writer per session, and the single-writer lease enforces it
//! across processes. On a machine running more than one agent client that was an
//! **outage**: each client spawned its own `lambo serve` per the documented
//! stdio wiring, the lease admitted one, and the rest exited 1 — in one client's
//! case with no error reaching the agent at all. Agents never clash; serve
//! processes do.
//!
//! J2's answer is that a refused serve becomes a thin proxy to the holder. For
//! that to be possible the holder has to be *reachable*, and reachability stops
//! being a transport choice: a stdio holder binds a local unix socket too, and
//! publishes its address into `session_leases.endpoint` (see
//! [`crate::store::lease`]).
//!
//! ## The path is derived, not chosen
//!
//! [`SessionEndpoint::resolve`] is a pure function of the session id and the
//! store's identity, plus environment. It performs **no I/O**, which is what
//! lets `serve` refuse an unusable endpoint *before* it takes the lease —
//! joining `authorize_bind` and `authorize_ledger` in the pre-lease group whose
//! whole point is that a misconfigured start costs nothing and leaves no lease
//! behind.
//!
//! **The store discriminator is load-bearing.** Two `lambo serve` processes with
//! the same `--session` but different `lambo.toml` stores are two different
//! graphs. They win two different lease rows and neither refuses the other, so a
//! path keyed on the session alone would have them fight over one socket — and
//! would let a proxy forward calls into the wrong graph. Hashing the store's
//! identity into the filename makes those two sessions two endpoints. The hash
//! is also what keeps a DSN (which can carry a password) out of both the
//! filesystem and the lease row.
//!
//! The published value is still read from the row rather than assumed, because
//! the row is the authority on where *this* holder listens: a proxy compares the
//! row against its own derivation and refuses honestly when they disagree,
//! rather than dialling a path the holder never bound.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::store::{StoreConfig, StoreKind};
use crate::types::LamboError;

/// The `sun_path` bound a unix-domain socket address must fit in, **including**
/// its NUL terminator.
///
/// 104 is macOS/BSD; Linux allows 108. The tighter of the two is used on every
/// platform on purpose: a path that works here works everywhere, and a session
/// name that only fits on Linux would be a portability trap discovered by a
/// colleague rather than by this check.
const SUN_PATH_MAX: usize = 104;

/// How much of the session name is kept in the filename, for a human reading
/// `ls` output. Identity comes from the hash, never from this prefix, so
/// truncating it cannot collide two sessions.
///
/// The filename is therefore at most `16 + 1 + 16 + 5 = 38` bytes, which is
/// what makes the whole path fit [`SUN_PATH_MAX`] on the tightest real base
/// directory measured: macOS's per-user `TMPDIR` is a fixed-shape
/// `/var/folders/XX/<28>/T/` (46 bytes), plus `lambo/` (6) plus 38 is 90, one
/// short of 91 with the NUL — 13 bytes of headroom. Widening this constant
/// spends that headroom, so it is a deliberate act, not a tidy-up.
const SESSION_PREFIX_CHARS: usize = 16;

/// Where a holder listens, and the string it publishes into the lease row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEndpoint {
    path: PathBuf,
}

impl SessionEndpoint {
    /// This session's endpoint, or `None` when the store cannot be shared
    /// between processes at all.
    ///
    /// **A process-private store has no hub worth advertising.** `MemoryStore`
    /// keeps its lease in a per-instance map and an in-memory SQLite database is
    /// private to the connection that opened it, so two `lambo serve` processes
    /// pointed at either one are two unrelated graphs: each wins its own lease,
    /// neither ever refuses the other, and a proxy that somehow reached across
    /// would be forwarding writes into the wrong graph. Worse, `store_identity`
    /// cannot tell two such stores apart — they have no address — so a derived
    /// path would be the *same* for both, and the second holder's stale-socket
    /// cleanup would unlink the first's live socket.
    ///
    /// So: no endpoint, no bind, nothing published to the row, and a refused
    /// serve on such a store behaves exactly as it did before J2. That is not a
    /// regression, because the multi-client outage J2 fixes cannot occur on a
    /// store no second process can see.
    pub fn for_store(session: &str, store: &StoreConfig) -> Result<Option<Self>, LamboError> {
        if !store_is_shareable(store) {
            return Ok(None);
        }
        Self::resolve(session, store).map(Some)
    }

    /// Derive this session's endpoint. **Pure** apart from reading environment;
    /// no directory is created and nothing is bound, so a caller can refuse an
    /// unusable one before taking any lease.
    ///
    /// Fails only when the derived path cannot fit [`SUN_PATH_MAX`], which is a
    /// property of the *base directory*, not of the session name — the name's
    /// contribution is bounded by construction. The message therefore points at
    /// the thing the operator can change.
    pub fn resolve(session: &str, store: &StoreConfig) -> Result<Self, LamboError> {
        Self::resolve_in(&endpoint_dir(), session, store)
    }

    /// [`SessionEndpoint::resolve`] with the base directory supplied.
    ///
    /// Split out so the derivation is testable without touching process-global
    /// environment: `set_var` is shared by every test in the binary, and two
    /// tests racing on `XDG_RUNTIME_DIR` is exactly the kind of flake that reads
    /// as a real defect on a loaded CI runner.
    fn resolve_in(dir: &Path, session: &str, store: &StoreConfig) -> Result<Self, LamboError> {
        // Identity is the hash over BOTH halves. The session must be in it: two
        // sessions on one store differ only by the cosmetic prefix otherwise,
        // and that prefix is truncated, so two long names sharing their first
        // characters would land on one socket. (Found by this module's own
        // `sessions_sharing_a_truncated_prefix_do_not_collide`.)
        let file = format!(
            "{}-{:016x}.sock",
            sanitize_prefix(session),
            fnv1a64(&format!("{session}\u{1f}{}", store_identity(store)))
        );
        let path = dir.join(file);
        let len = path.as_os_str().as_encoded_bytes().len();
        if len + 1 > SUN_PATH_MAX {
            return Err(LamboError::Config(format!(
                "refusing to start: this session's local endpoint would be {len} bytes, over the \
                 {SUN_PATH_MAX}-byte limit a unix socket address has room for. The session name \
                 is not the problem — its contribution is bounded — the base directory is. Set \
                 XDG_RUNTIME_DIR (or TMPDIR) to a shorter path and retry."
            )));
        }
        Ok(Self { path })
    }

    /// The filesystem path to bind or dial.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bind this endpoint, making the holder reachable.
    ///
    /// **Called only after the single-writer lease has been won, and that
    /// ordering is load-bearing twice over.**
    ///
    /// 1. `authorize_bind`'s reason for running first — "refusing here means no
    ///    lease is taken, so the operator's retry is not blocked by the lease
    ///    their own refused start would otherwise be holding" — stays literally
    ///    true. A serve that loses the lease binds nothing and has nothing to
    ///    clean up. J2 publishes the endpoint *string* with the acquire (it is
    ///    derived, so its value needs no bind to exist) and binds afterwards,
    ///    which is what let the unconditional-binding requirement land without
    ///    falsifying that sentence.
    /// 2. **The lease is what licenses the unlink below.** A socket file already
    ///    at this path, while we hold the lease, cannot belong to a live holder —
    ///    the lease admits one. So it is the leftover of a crashed one, and
    ///    removing it is safe. Unlinking *before* winning the lease would delete
    ///    a healthy hub's socket out from under it.
    ///
    /// The directory is created 0700 and **its permissions are checked**, which
    /// is what makes the shared `/tmp` fallback safe rather than assumed safe: a
    /// directory an attacker pre-created world-writable is refused rather than
    /// bound into. A directory they own with 0700 fails our bind with `EACCES`,
    /// which is also a refusal. Same-uid processes are not in the threat model —
    /// they can already read the store.
    pub fn bind(&self) -> Result<tokio::net::UnixListener, LamboError> {
        let dir = self.path.parent().ok_or_else(|| {
            LamboError::Config(format!(
                "endpoint {} has no parent directory",
                self.path.display()
            ))
        })?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| {
                LamboError::Config(format!(
                    "endpoint directory {} could not be created: {e}",
                    dir.display()
                ))
            })?;
        let mode = std::fs::metadata(dir)
            .map_err(|e| {
                LamboError::Config(format!(
                    "endpoint directory {} could not be inspected: {e}",
                    dir.display()
                ))
            })?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(LamboError::Config(format!(
                "refusing to bind the session endpoint: {} is mode {mode:o}, reachable by other                  users. A socket there would let any local account issue writes against this                  session. Remove or chmod 700 that directory, or set XDG_RUNTIME_DIR to a                  private one.",
                dir.display()
            )));
        }

        let listener = match tokio::net::UnixListener::bind(&self.path) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Licensed by the lease — see this function's docs. A live
                // holder cannot be here, so this file is a crashed one's.
                tracing::warn!(
                    endpoint = %self.path.display(),
                    "lambo serve: a stale session endpoint was left by an earlier holder —                      removing it (this process holds the single-writer lease, so no live holder                      can be listening there)"
                );
                let _ = std::fs::remove_file(&self.path);
                tokio::net::UnixListener::bind(&self.path).map_err(|e| {
                    LamboError::Config(format!(
                        "session endpoint {} could not be bound even after clearing a stale                          socket: {e}",
                        self.path.display()
                    ))
                })?
            }
            Err(e) => {
                return Err(LamboError::Config(format!(
                    "session endpoint {} could not be bound: {e}",
                    self.path.display()
                )))
            }
        };
        // Defence in depth beside the directory mode: even in a shared
        // directory the socket itself is owner-only.
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        Ok(listener)
    }

    /// Remove the socket file on the way out, best effort.
    ///
    /// Not required for correctness — the next holder's [`SessionEndpoint::bind`]
    /// clears a stale socket under the lease's licence — but a clean exit should
    /// not leave a file behind that makes the *next* start log a stale-socket
    /// warning it did not earn.
    pub fn unlink(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::debug!(
                endpoint = %self.path.display(),
                "lambo serve: session endpoint removed"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                endpoint = %self.path.display(),
                error = %e,
                "lambo serve: session endpoint could not be removed; the next holder will clear it"
            ),
        }
    }

    /// The value written to `session_leases.endpoint`.
    ///
    /// A path, not a URL: it names a socket on the **holder's own machine**.
    /// `session_leases.holder` carries the host, which is what a reader on a
    /// different host must check before believing this string means anything to
    /// it.
    pub fn published(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

/// The directory endpoints live in, in preference order.
///
/// * `$XDG_RUNTIME_DIR/lambo` — the correct home on Linux: already per-user,
///   already 0700, cleaned up at logout.
/// * `$TMPDIR/lambo` — the macOS answer, where `XDG_RUNTIME_DIR` is unset and
///   `TMPDIR` is already a per-user `/var/folders/...` directory.
/// * `/tmp/lambo` — the last resort, and the only one that is *shared*. The
///   directory-permission check in [`SessionEndpoint::bind`] is what makes this
///   fallback safe rather than assumed-safe.
fn endpoint_dir() -> PathBuf {
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Ok(base) = std::env::var(var) {
            let base = base.trim_end_matches('/');
            if !base.is_empty() {
                return PathBuf::from(base).join("lambo");
            }
        }
    }
    PathBuf::from("/tmp/lambo")
}

/// A filesystem-safe, length-bounded prefix of the session name — for a human
/// reading `ls`, never for identity.
fn sanitize_prefix(session: &str) -> String {
    let kept: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(SESSION_PREFIX_CHARS)
        .collect();
    if kept.is_empty() {
        "session".to_string()
    } else {
        kept
    }
}

/// What makes two stores different stores, as a string to be hashed.
///
/// Deliberately *not* published anywhere: a DSN can carry a password, and the
/// point of hashing is that neither the filesystem nor the lease row ever holds
/// one.
fn store_identity(store: &StoreConfig) -> String {
    format!(
        "{:?}\u{1f}{}\u{1f}{}",
        store.kind,
        store.dsn.as_deref().unwrap_or(""),
        store.path.as_deref().unwrap_or("")
    )
}

/// Can a second process see this store at all?
///
/// The one question [`SessionEndpoint::for_store`] asks. `false` for the in-RAM
/// adapter and for an in-memory SQLite database, which are private to the
/// process (and to the connection) that opened them.
fn store_is_shareable(store: &StoreConfig) -> bool {
    match store.kind {
        StoreKind::Memory => false,
        StoreKind::Cockroach => true,
        // The in-memory spellings SQLite accepts: the bare `:memory:`, the
        // `sqlite::memory:` URL this crate uses, and the `mode=memory` URI
        // parameter. Anything else is a file another process can open.
        StoreKind::Sqlite => {
            let path = store.path.as_deref().unwrap_or_default();
            !(path.contains(":memory:") || path.contains("mode=memory"))
        }
    }
}

/// FNV-1a, 64-bit — written out rather than taken from `DefaultHasher`.
///
/// `std`'s `DefaultHasher` is explicitly **not** stable across Rust releases,
/// and this hash is baked into a filesystem path two processes must agree on. A
/// compiler upgrade must not move a session's endpoint out from under a running
/// holder.
fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: StoreKind, path: Option<&str>, dsn: Option<&str>) -> StoreConfig {
        StoreConfig {
            kind,
            dsn: dsn.map(str::to_string),
            path: path.map(str::to_string),
            ..StoreConfig::default()
        }
    }

    /// A short, fixed base directory, so these tests assert on the derivation
    /// rather than on whatever `TMPDIR` this machine happens to have.
    fn at(session: &str, store: &StoreConfig) -> SessionEndpoint {
        SessionEndpoint::resolve_in(Path::new("/run/lambo"), session, store).unwrap()
    }

    #[test]
    fn the_same_session_on_two_stores_is_two_endpoints() {
        let a = at("lambo-dev", &cfg(StoreKind::Sqlite, Some("/a.db"), None));
        let b = at("lambo-dev", &cfg(StoreKind::Sqlite, Some("/b.db"), None));
        assert_ne!(
            a, b,
            "two stores under one session name are two graphs; sharing a socket would let a \
             proxy forward into the wrong one"
        );
        // And the same store is the same endpoint, or a proxy could never find
        // the holder it just lost to.
        assert_eq!(
            a,
            at("lambo-dev", &cfg(StoreKind::Sqlite, Some("/a.db"), None))
        );
        // A cockroach DSN is a different store identity from a sqlite path even
        // when neither is set on the other side.
        assert_ne!(
            a,
            at(
                "lambo-dev",
                &cfg(StoreKind::Cockroach, None, Some("postgres://x/y"))
            )
        );
    }

    #[test]
    fn two_sessions_on_one_store_are_two_endpoints() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        assert_ne!(at("alpha", &store), at("beta", &store));
    }

    /// The prefix is cosmetic, so two sessions sharing one must still differ —
    /// identity lives entirely in the hash.
    #[test]
    fn sessions_sharing_a_truncated_prefix_do_not_collide() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let long_a = "a-very-long-session-name-one";
        let long_b = "a-very-long-session-name-two";
        assert_eq!(sanitize_prefix(long_a), sanitize_prefix(long_b));
        assert_ne!(at(long_a, &store), at(long_b, &store));
    }

    #[test]
    fn a_hostile_session_name_cannot_escape_the_endpoint_directory() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let ep = at("../../etc/passwd", &store);
        assert_eq!(ep.path().parent().unwrap(), Path::new("/run/lambo"));
        let name = ep.path().file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('/'), "no separator survives: {name}");
        assert!(!name.contains(".."), "no traversal survives: {name}");
    }

    #[test]
    fn an_over_long_base_directory_is_refused_before_any_lease() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        let err = SessionEndpoint::resolve_in(&long, "s", &store)
            .expect_err("a base directory this long cannot hold a socket address");
        let msg = err.to_string();
        assert!(msg.contains("104"), "names the limit: {msg}");
        assert!(
            msg.contains("XDG_RUNTIME_DIR"),
            "names what the operator can change: {msg}"
        );
    }

    /// The real derivation on the real environment must fit on THIS machine —
    /// the headroom arithmetic at [`SESSION_PREFIX_CHARS`] is a claim about a
    /// measured base directory, and this is what keeps it honest.
    #[test]
    fn the_ambient_environment_yields_a_bindable_endpoint() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let ep = SessionEndpoint::resolve("lambo-dev", &store)
            .expect("this machine's base directory must hold a socket address");
        assert!(ep.published().ends_with(".sock"));
        // `< SUN_PATH_MAX`, i.e. `len + 1 <= SUN_PATH_MAX` — the NUL terminator
        // is what the bound has to leave room for.
        assert!(ep.path().as_os_str().as_encoded_bytes().len() < SUN_PATH_MAX);
    }

    /// A store no second process can see gets no endpoint at all — see
    /// `for_store`. Binding one would be worse than useless: two such holders
    /// derive the SAME path (they have no address to hash) and the second one's
    /// stale-socket cleanup would unlink the first's live socket.
    #[test]
    fn a_process_private_store_advertises_no_endpoint() {
        for store in [
            cfg(StoreKind::Memory, None, None),
            cfg(StoreKind::Sqlite, Some("sqlite::memory:"), None),
            cfg(StoreKind::Sqlite, Some(":memory:?cache=shared"), None),
            cfg(StoreKind::Sqlite, Some("file:x?mode=memory"), None),
        ] {
            assert_eq!(
                SessionEndpoint::for_store("s", &store).unwrap(),
                None,
                "{store:?} is private to this process"
            );
        }
        // A real file, and a cluster, are both reachable by a second process.
        assert!(
            SessionEndpoint::for_store("s", &cfg(StoreKind::Sqlite, Some("/a.db"), None))
                .unwrap()
                .is_some()
        );
        assert!(SessionEndpoint::for_store(
            "s",
            &cfg(StoreKind::Cockroach, None, Some("postgres://h/db"))
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn the_hash_is_pinned_so_a_compiler_upgrade_cannot_move_an_endpoint() {
        // FNV-1a 64 of the empty string is its offset basis, and of "a" the
        // basis times the prime. Pinned literally: this hash is baked into a
        // path two processes must agree on across builds.
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
    }
}
