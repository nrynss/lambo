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
//! [`SessionEndpoint::resolve`] is a function of the session id and the store's
//! **identity**, plus environment. It **creates nothing and binds nothing** —
//! the only filesystem access it makes is the read-only `canonicalize` that
//! turns a store path into a store identity (see `store_identity`), which
//! leaves nothing behind on any code path. That is what lets `serve` derive an
//! endpoint *before* it takes the lease, beside `authorize_bind` and
//! `authorize_ledger`, whose group exists so that a misconfigured start costs
//! nothing and leaves no lease behind.
//!
//! (Before J2's round-1 remediation this said "performs **no I/O**", and the
//! sun_path length check was a pre-lease *refusal*. Both changed: the identity
//! needs a stat to be an identity at all — J2-R1-2 — and an unusable endpoint
//! now degrades to `None` rather than refusing the start — J2-R1-5.)
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
//! **And identity is not spelling** (J2-R1-2). `path = "./lambo.db"` — the
//! spelling every published example uses — names a *different file* from every
//! different cwd, because `SqliteConnectOptions` resolves a relative path
//! against each process's own working directory. Hashing it verbatim gave two
//! graphs **one** socket, at which point the second holder's stale-socket unlink
//! removes the first holder's live socket and a proxy of graph A forwards writes
//! into graph B — verbatim the outcome this discriminator exists to prevent. So
//! the file half of the identity is canonicalized before it is hashed.
//!
//! The published value is still read from the row rather than assumed, because
//! the row is the authority on where *this* holder listens: a proxy compares the
//! row against its own derivation and refuses honestly when they disagree,
//! rather than dialling a path the holder never bound.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
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
/// The filename is therefore at most `16 + 1 + 16 + 5 = 38` bytes. Against the
/// two base directories [`endpoint_dir`] can produce:
///
/// * `/tmp/lambo-<euid>/` — 15 bytes for a 3-digit uid, 18 for a 6-digit one,
///   plus 38 is 53 to 56, or 54 to 57 with the NUL: **47 to 50 bytes of
///   headroom**;
/// * `$XDG_RUNTIME_DIR/lambo/` — unbounded in principle, `/run/user/<uid>/` in
///   practice, which is 21 bytes for a 5-digit uid, so 60 with the NUL and 44
///   bytes of headroom.
///
/// Widening this constant spends that headroom, so it is a deliberate act, not a
/// tidy-up. Dropping the `TMPDIR` rung (J2-L1) is what made the arithmetic
/// comfortable: macOS's `TMPDIR` is a fixed-shape `/var/folders/XX/<28>/T/`
/// (46 bytes) and left only 6 to 9 bytes.
/// `the_ambient_environment_yields_a_bindable_endpoint` keeps this honest on
/// whatever machine runs the suite, since `XDG_RUNTIME_DIR` can be anything.
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
    ///
    /// # `None` is also what an unusable path gets (J2-R1-5)
    ///
    /// An over-long `sun_path` used to propagate out of here and **stop the
    /// serve**. Two roads to the same operator situation — "this process cannot
    /// have an endpoint" — then ended in opposite outcomes: a *failed bind*
    /// deliberately does not stop the process ("a bind failure does not stop
    /// this process serving memory — the same posture `Ledger::open` takes"),
    /// while a base directory too long for a socket address was fatal. The
    /// harsher outcome was attached to the cheaper problem, and it made a long
    /// runtime directory (a deep per-user path, a container mount, a long
    /// username) a hard startup failure on a machine that served fine before J2,
    /// for a feature the operator never asked for.
    ///
    /// The pre-lease argument was never about the refusal being fatal — it is
    /// about *where* the check may live, since it leaves nothing behind. So the
    /// check stays here and its message is unchanged; only the outcome changes.
    /// The consequence, stated: on such a machine a losing serve refuses as it
    /// did before J2, because a proxy needs the holder to have bound. That is
    /// the correct degradation — one client keeps working instead of none.
    pub fn for_store(session: &str, store: &StoreConfig) -> Option<Self> {
        Self::for_store_in(&endpoint_dir(), session, store)
    }

    /// [`SessionEndpoint::for_store`] with the base directory supplied, for the
    /// same reason [`SessionEndpoint::resolve_in`] exists.
    fn for_store_in(dir: &Path, session: &str, store: &StoreConfig) -> Option<Self> {
        if !store_is_shareable(store) {
            return None;
        }
        match Self::resolve_in(dir, session, store) {
            Ok(endpoint) => Some(endpoint),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "lambo serve: this session can have no local endpoint — this process still \
                     serves its own client normally, but other clients on this machine CANNOT \
                     attach to this session, and a serve that loses the lease here will refuse \
                     as it did before J2 instead of proxying"
                );
                None
            }
        }
    }

    /// Derive this session's endpoint. **Creates nothing and binds nothing**, so
    /// a caller can call it before taking any lease; the one filesystem access
    /// is `store_identity`'s read-only `canonicalize`.
    ///
    /// Fails only when the derived path cannot fit [`SUN_PATH_MAX`], which is a
    /// property of the *base directory*, not of the session name — the name's
    /// contribution is bounded by construction. The message therefore points at
    /// the thing the operator can change. [`SessionEndpoint::for_store`] turns
    /// that failure into `None` rather than a refused start (J2-R1-5); this
    /// function still reports it, because a caller that wants the reason should
    /// be able to have it.
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
                "this session can have no local endpoint: the address would be {len} bytes, over \
                 the {SUN_PATH_MAX}-byte limit a unix socket address has room for. The session \
                 name is not the problem — its contribution is bounded — the base directory is. \
                 Unset XDG_RUNTIME_DIR to fall back to a short private directory under tmp, or \
                 point it at a shorter path, so other clients on this machine can attach to \
                 this session."
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
    /// The directory is created 0700 and then **checked three ways** — it is not
    /// a symlink, it is owned by this euid, and its mode grants nothing to group
    /// or other. Together with the per-uid name (see [`endpoint_dir`]) that is
    /// what makes the shared `/tmp` fallback safe rather than assumed safe.
    ///
    /// Each check answers a distinct attack, and the first two were added by
    /// J2-R1-3:
    ///
    /// * **Not a symlink.** The mode was previously read with
    ///   `std::fs::metadata`, which *follows* symlinks, so an attacker-placed
    ///   `/tmp/lambo-<uid> → /tmp/theirs` with `/tmp/theirs` at 0700 passed the
    ///   mode gate and we bound a socket inside a directory they control.
    ///   `symlink_metadata` asks about the entry itself.
    /// * **Owned by us.** A 0700 directory owned by a *different* uid that we
    ///   can nonetheless write into — an ACL grant on macOS, a group-writable
    ///   ancestor — passed the mode gate too. The old docstring claimed the mode
    ///   check was "what makes the shared /tmp fallback safe"; for that case it
    ///   was not.
    /// * **Mode 0700.** A directory an attacker pre-created world-writable is
    ///   refused rather than bound into.
    ///
    /// Same-uid processes remain out of the threat model — they can already read
    /// the store.
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
        assert_private_dir(dir, "bind the session endpoint")?;

        let listener = match tokio::net::UnixListener::bind(&self.path) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Licensed by the lease — see this function's docs. A live
                // holder cannot be here, so this file is a crashed one's.
                tracing::warn!(
                    endpoint = %self.path.display(),
                    "lambo serve: a stale session endpoint was left by an earlier holder — \
                     removing it (this process holds the single-writer lease, so no \
                     live holder can be listening there)"
                );
                let _ = std::fs::remove_file(&self.path);
                tokio::net::UnixListener::bind(&self.path).map_err(|e| {
                    LamboError::Config(format!(
                        "session endpoint {} could not be bound even after clearing a \
                         stale socket: {e}",
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

    /// The `(device, inode)` of the socket file at this path right now, or
    /// `None` if nothing is there.
    ///
    /// Taken immediately after a successful [`SessionEndpoint::bind`] and
    /// handed back to [`SessionEndpoint::unlink_if_ours`] at exit — see there
    /// for why the *path* is not enough to identify our own socket.
    pub fn file_identity(&self) -> Option<(u64, u64)> {
        std::fs::symlink_metadata(&self.path)
            .ok()
            .map(|m| (m.dev(), m.ino()))
    }

    /// Remove the socket file on the way out — **but only when it is still the
    /// one this process bound** (JE2E-2).
    ///
    /// Not required for correctness — the next holder's [`SessionEndpoint::bind`]
    /// clears a stale socket under the lease's licence — but a clean exit should
    /// not leave a file behind that makes the *next* start log a stale-socket
    /// warning it did not earn.
    ///
    /// # Why the path is not a licence, and the lease is not one either
    ///
    /// The address is a **pure function** of session and store, so every holder
    /// generation binds the same path. "The lease is what licenses the
    /// stale-socket unlink" is true at `bind` — while we hold the lease, a file
    /// at this path cannot belong to a live holder — and it is *not* true here,
    /// because the exit path runs after the lease has stopped being ours:
    ///
    /// * A **fenced** ex-holder (its lease expired, another writer took the
    ///   session) reaches its exit still holding a `SessionEndpoint` for a path
    ///   the *new* holder is now listening on. Removing it silently disabled
    ///   multi-client attach for the new holder's whole lifetime, with every
    ///   later loser told the holder "has most likely died" — the original J
    ///   outage recreated by a race, with a misleading refusal on top.
    /// * A **clean** close releases the lease *before* this runs, so a new
    ///   holder can lawfully win it and bind in the gap. The window is small;
    ///   it is not zero.
    ///
    /// So the licence is identity, not authority: `bound` is the `(dev, ino)`
    /// this process saw the instant after it bound, and the file is removed only
    /// while the path still resolves to it. A new holder's `bind` unlinks the
    /// old file and creates a new inode, so a superseded endpoint's identity can
    /// never match and its owner can never delete a live successor's socket —
    /// by construction rather than by an argument about who holds what.
    ///
    /// `None` means the bind never happened (or the stat failed), and nothing is
    /// removed: this process put no file here to clean up.
    pub fn unlink_if_ours(&self, bound: Option<(u64, u64)>) {
        let Some(bound) = bound else { return };
        match self.file_identity() {
            Some(now) if now == bound => {}
            Some(_) => {
                tracing::info!(
                    endpoint = %self.path.display(),
                    "lambo serve: the session endpoint at this path is no longer the socket this \
                     process bound — another holder has taken the session and bound its own, so \
                     it is left alone (removing it would silently disable multi-client attach for \
                     the new holder)"
                );
                return;
            }
            None => return,
        }
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

/// The directory endpoints live in — **two rungs, deliberately, not three.**
///
/// * `$XDG_RUNTIME_DIR/lambo` — the correct home on Linux: already per-user,
///   already 0700, cleaned up at logout, and on a system using `PrivateTmp` it
///   is the *only* rung two processes can share. **No uid suffix, because the
///   base directory already carries one** (`/run/user/<uid>`), and spending path
///   bytes on a discriminator that is already there would eat the `sun_path`
///   headroom for nothing.
/// * `/tmp/lambo-<euid>` — everywhere else: macOS, bare containers, `ssh`
///   without `pam_systemd`, cron.
///
/// # Why the fallback is per-uid (J2-R1-3)
///
/// The suffix is what makes the shared fallback safe **by construction** rather
/// than by check, and losing it was a regression, not merely a missed hardening.
/// Without it the first uid to run `lambo serve` creates `/tmp/lambo` mode 0700
/// owned by itself, and every other uid's `bind` then fails `EACCES` — for every
/// holder, since every holder binds. `/tmp`'s sticky bit means the second user
/// cannot even clear it. A case that worked fine before J2 becomes a hard
/// cross-user lockout, curable only by the first user logging in.
///
/// This restores the decision recorded in the project graph, which the shipped
/// stage-2 code dropped on both fallbacks; the graph and the code now agree.
///
/// # Why `TMPDIR` was removed (J2-L1)
///
/// `TMPDIR` was the second of three rungs, and the live two-client probe showed
/// it is **the wrong kind of variable to key a shared address on**: it varies
/// per *client product*, by accident, for one user on one machine. Measured on
/// macOS with `XDG_RUNTIME_DIR` unset in both children:
///
/// * `cursor-agent` **scrubs** `TMPDIR` from the environment of the MCP server
///   it spawns → the derivation fell through to `/tmp/lambo`;
/// * `opencode` **passes** macOS's per-user `TMPDIR` through →
///   `$TMPDIR/lambo`.
///
/// Same binary, same store, same session, two addresses. The losing serve
/// compared the row's published endpoint against its own derivation, refused to
/// forward ("it is running a different endpoint scheme"), waited out its
/// election budget, and the client declared the server failed. Cross-client
/// memory was silently absent on **unmodified default wiring** — the exact
/// failure J2 exists to remove, reintroduced through the environment.
///
/// Two rungs make that case disappear at the source: with `XDG_RUNTIME_DIR`
/// unset, *every* client lands on `/tmp/lambo-<euid>` no matter what it does
/// with `TMPDIR`. `XDG_RUNTIME_DIR` stays because it is a different kind of
/// variable — set once per login session by the platform, not per child by a
/// client — and because it is the rung that works where `/tmp` is not shared.
/// It can still be scrubbed by one client and not another, which is why
/// [`crate::mcp::proxy::proxyable`] no longer *requires* the directories to
/// match; nothing here has to be perfect, only unsurprising.
///
/// Losing the rung costs nothing else: `/tmp/lambo-<euid>` is *shorter* than
/// macOS's `TMPDIR`, so it gives the `sun_path` bound more headroom rather than
/// less (see [`SESSION_PREFIX_CHARS`]), and privacy comes from the 0700 mode and
/// the ownership check either way.
fn endpoint_dir() -> PathBuf {
    // SAFETY: `geteuid` is always successful and touches no memory the caller
    // owns. There is no std equivalent.
    let uid = unsafe { libc::geteuid() };
    endpoint_dir_from(std::env::var("XDG_RUNTIME_DIR").ok().as_deref(), uid)
}

/// [`endpoint_dir`] with the environment supplied — pure, so the preference
/// order and the uid discriminator are testable without `set_var`, which is
/// process-global and makes two tests racing on an environment variable look
/// like a real defect on a loaded runner.
fn endpoint_dir_from(xdg: Option<&str>, uid: u32) -> PathBuf {
    if let Some(base) = xdg.and_then(non_empty_dir) {
        return base.join("lambo");
    }
    PathBuf::from(format!("/tmp/lambo-{uid}"))
}

/// A set, non-empty, trailing-slash-trimmed base directory.
fn non_empty_dir(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Refuse a directory that is not a private, self-owned one.
///
/// Shared by [`SessionEndpoint::bind`] and by the proxy's dial
/// ([`crate::mcp::proxy::dial_dir`]), and **symmetry is the point**: a directory
/// this process would refuse to *place* a socket in is one it must refuse to
/// *reach* a socket in, since J2-L1 lets a proxy dial the directory the holder
/// published rather than only its own derivation. `what` names the action for the
/// message, which is operator-facing on stderr.
///
/// Three checks, each answering a distinct attack (J2-R1-3):
///
/// * **Not a symlink.** `std::fs::metadata` *follows* symlinks, so an
///   attacker-placed `/tmp/lambo-<uid> → /tmp/theirs` with `/tmp/theirs` at 0700
///   passed a mode-only gate. `symlink_metadata` asks about the entry itself.
/// * **Owned by this euid.** A 0700 directory owned by a *different* uid that we
///   can nonetheless write into — an ACL grant on macOS, a group-writable
///   ancestor — passed a mode-only gate too.
/// * **Mode 0700.** A directory an attacker pre-created world-writable is
///   refused rather than used.
///
/// Same-uid processes remain out of the threat model — they can already read the
/// store.
pub(crate) fn assert_private_dir(dir: &Path, what: &str) -> Result<(), LamboError> {
    // `symlink_metadata`, not `metadata`: this must be a statement about this
    // directory entry, not about wherever a pre-placed symlink points.
    let meta = std::fs::symlink_metadata(dir).map_err(|e| {
        LamboError::Config(format!(
            "endpoint directory {} could not be inspected: {e}",
            dir.display()
        ))
    })?;
    if meta.file_type().is_symlink() {
        return Err(LamboError::Config(format!(
            "refusing to {what}: {} is a symbolic link, not a directory. Its \
             target's permissions say nothing about who can reach a socket placed \
             through it, so this process will not follow it. Remove that link, or \
             set XDG_RUNTIME_DIR to a private directory.",
            dir.display()
        )));
    }
    // SAFETY: as in `endpoint_dir` — `geteuid` cannot fail.
    let ours = unsafe { libc::geteuid() };
    if meta.uid() != ours {
        return Err(LamboError::Config(format!(
            "refusing to {what}: {} is owned by uid {}, not by this process's uid \
             {ours}. Even at mode 700 its owner controls what is in it, so a socket \
             there is not this session's to trust. Remove that directory, or set \
             XDG_RUNTIME_DIR to one you own.",
            dir.display(),
            meta.uid()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(LamboError::Config(format!(
            "refusing to {what}: {} is mode {mode:o}, reachable by other users. A \
             socket there would let any local account issue writes against this \
             session. Remove or chmod 700 that directory, or set XDG_RUNTIME_DIR to \
             a private one.",
            dir.display()
        )));
    }
    Ok(())
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
///
/// The DSN half is taken verbatim — it is already an absolute, host-qualified
/// address. The path half is **canonicalized** first, because a path is a
/// spelling and this function must produce an identity; see
/// [`canonical_store_path`] and J2-R1-2.
fn store_identity(store: &StoreConfig) -> String {
    format!(
        "{:?}\u{1f}{}\u{1f}{}",
        store.kind,
        store.dsn.as_deref().unwrap_or(""),
        canonical_store_path(store.path.as_deref().unwrap_or(""))
    )
}

/// Turn a store path *spelling* into a store *identity* (J2-R1-2).
///
/// # Why this is not cosmetic
///
/// `path = "./lambo.db"` is what `docs/reference/config.mdx`,
/// `installation.mdx`, `end-to-end.mdx` and `lambo.example.toml` all show, and
/// `SqliteConnectOptions::from_str` resolves it against **each process's own
/// cwd**. Two agent clients launched from two directories with the documented
/// config are therefore two different SQLite files — and, before this function,
/// one derived socket. Both win their own lease (two databases, two rows), the
/// second holder's `bind` takes the `AddrInUse` branch and unlinks the *first
/// holder's live socket*, and a proxy belonging to graph A then dials the path
/// and reaches holder B. The licence [`SessionEndpoint::bind`] argues for that
/// unlink — "while we hold the lease, a socket file at this path cannot belong
/// to a live holder" — is only true when the path is unique per store, which is
/// what this restores.
///
/// # The rule, and what it decides
///
/// * **Symlinks are resolved — once their target exists.** `std::fs::canonicalize`
///   on an existing file means the same store reached by a symlink and reached
///   directly derives **one** address. That is the deliberate choice: one store
///   must be one socket, or the second holder unlinks the first's. The cost is
///   that two spellings which *look* different are correctly treated as one
///   thing, which is the point.
///
///   The qualifier is load-bearing and the claim used to be stated without it
///   (J2-R2-5). `canonicalize` requires the **whole** path to exist, and
///   `realpath(3)` fails with `ENOENT` on a *dangling* symlink, so a link whose
///   target has not been created yet takes the not-exists branch below and
///   resolves to the **link's own** name. `create_if_missing` then writes
///   through the link and creates the target, and the next process resolves to
///   the **target's** name: one store, two identities, one on each side of the
///   file's creation. The consequence is a `proxyable` refusal
///   (`EndpointIsNotOurs`) whose message blames a different session, store or
///   scheme — none of which is true — and it degrades safely, because the lease
///   still serialises the writers and no graph is at risk. Narrowed rather than
///   closed: resolving the link chain by hand would put a second, subtly
///   different path resolver beside `canonicalize` for a configuration
///   (a store reached through a symlink to a file that does not exist yet) that
///   no documented wiring produces.
/// * **A file that does not exist yet** is resolved through its parent
///   directory, keeping the file name literal. The parent is where a relative
///   path's ambiguity lives, so this is enough to make `./lambo.db` from two
///   cwds two identities and `./lambo.db` and `/abs/cwd/lambo.db` one. It also
///   matters in practice: `SqliteStore::connect` builds a *lazy* pool with
///   `create_if_missing`, so the file often does not exist when a serve derives
///   its endpoint.
/// * **Neither resolves** — the parent directory is missing too — and the value
///   is used as-is after `cwd.join`. This is best-effort by construction: SQLite
///   cannot create a file in a directory that does not exist, so a process on
///   this branch is failing for a louder reason a moment later.
/// * **A URI spelling is kept verbatim.** `file:x?mode=rwc`, `sqlite://…`: these
///   are not filesystem paths, `canonicalize` would fail on them, and the
///   `cwd.join` fallback would then make one store's identity depend on the cwd
///   — reintroducing this very bug from the other side. (The in-memory
///   spellings never reach here: [`store_is_shareable`] rejects them first.)
fn canonical_store_path(raw: &str) -> String {
    if raw.is_empty() || looks_like_uri(raw) {
        return raw.to_string();
    }
    let p = Path::new(raw);
    if let Ok(resolved) = std::fs::canonicalize(p) {
        return resolved.to_string_lossy().into_owned();
    }
    // Not there yet. The parent carries the ambiguity, so resolve that and keep
    // the file name as written.
    if let Some(name) = p.file_name() {
        let parent = match p.parent() {
            // `Path::new("lambo.db").parent()` is `Some("")`, which is the cwd.
            Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
            Some(parent) => parent,
            None => Path::new("."),
        };
        if let Ok(resolved) = std::fs::canonicalize(parent) {
            return resolved.join(name).to_string_lossy().into_owned();
        }
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().into_owned(),
        Err(_) => raw.to_string(),
    }
}

/// Is this store path a URI spelling rather than a filesystem path?
///
/// Conservative on purpose: anything that might be a URI is left alone, because
/// the failure mode of canonicalizing a URI (a cwd-dependent identity) is the
/// bug [`canonical_store_path`] exists to fix.
fn looks_like_uri(raw: &str) -> bool {
    raw.contains('?') || raw.starts_with("file:") || raw.starts_with("sqlite:")
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

    /// A unique scratch directory for the tests that need real files — the
    /// canonicalization ones do, since that is the whole point of them.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lambo-endpoint-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A scratch directory short enough to hold a socket address — the bind
    /// tests need a real directory AND a path under [`SUN_PATH_MAX`], and macOS's
    /// `TMPDIR` is 46 bytes before anything is joined to it.
    fn short_scratch(tag: &str) -> PathBuf {
        let dir = PathBuf::from(format!(
            "/tmp/lb{tag}{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 100_000
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn path_of(root: &Path, sub: &str) -> String {
        root.join(sub)
            .join("lambo.db")
            .to_string_lossy()
            .into_owned()
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

    /// The derivation still *reports* an unusable base directory, with the
    /// message pointing at the thing the operator can change.
    #[test]
    fn an_over_long_base_directory_is_reported_by_the_derivation() {
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

    /// J2-R1-5: but it must not stop the serve.
    ///
    /// A *failed bind* deliberately does not — "a bind failure does not stop
    /// this process serving memory" — and this is the same operator situation
    /// reached by a cheaper road, so it degrades the same way. The consequence
    /// is that a losing serve on such a machine refuses as it did before J2,
    /// which is one client working instead of none.
    #[test]
    fn an_unusable_base_directory_degrades_to_no_endpoint_rather_than_refusing() {
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        assert_eq!(
            SessionEndpoint::for_store_in(&long, "s", &store),
            None,
            "an over-long base directory must cost this process its endpoint, not its start"
        );
        // And a usable one still yields an endpoint through the same door.
        assert!(SessionEndpoint::for_store_in(Path::new("/run/lambo"), "s", &store).is_some());
    }

    /// J2-R1-2, the headline case: `path = "./lambo.db"` is what every published
    /// example shows, and it names a **different file** from every different
    /// cwd.
    ///
    /// Asserted by composition rather than by `set_current_dir`, which is
    /// process-global and would race every other test in this binary: a relative
    /// spelling resolves to *this* process's cwd, so two processes with two cwds
    /// resolve to two paths — and `two_stores_with_one_file_name_in_two_
    /// directories_are_two_endpoints` shows two such paths are two endpoints.
    #[test]
    fn a_relative_store_path_is_resolved_against_this_process_cwd() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let want = cwd.join("lambo.db").to_string_lossy().into_owned();
        assert_eq!(canonical_store_path("./lambo.db"), want);
        assert_eq!(canonical_store_path("lambo.db"), want);
        // Before the fix this was the literal string, identical in every process
        // regardless of cwd — which is how two graphs got one socket.
        assert_ne!(canonical_store_path("./lambo.db"), "./lambo.db");
    }

    /// Two graphs, two sockets. Before the fix these two collided, and the
    /// second holder's stale-socket unlink removed the first holder's live
    /// socket.
    #[test]
    fn two_stores_with_one_file_name_in_two_directories_are_two_endpoints() {
        let root = scratch("two-dirs");
        for sub in ["a", "b"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
            std::fs::write(root.join(sub).join("lambo.db"), b"x").unwrap();
        }
        let a = at(
            "s",
            &cfg(StoreKind::Sqlite, Some(&path_of(&root, "a")), None),
        );
        let b = at(
            "s",
            &cfg(StoreKind::Sqlite, Some(&path_of(&root, "b")), None),
        );
        assert_ne!(a, b, "two SQLite files must never derive one socket");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One graph, one socket — the other half of the decision, and the reason
    /// symlinks are **resolved** rather than merely made absolute.
    ///
    /// Two spellings of one store must not derive two addresses: the loser's
    /// `proxyable` check would then refuse with a message blaming "a different
    /// lambo version, or a different XDG_RUNTIME_DIR", none of which is true.
    #[test]
    fn one_store_reached_two_ways_is_one_endpoint() {
        let root = scratch("one-store");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real").join("lambo.db"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let store = |p: PathBuf| cfg(StoreKind::Sqlite, Some(&p.to_string_lossy()), None);
        let direct = at("s", &store(root.join("real").join("lambo.db")));
        assert_eq!(
            direct,
            at("s", &store(root.join("link").join("lambo.db"))),
            "a store reached through a symlink is the same store"
        );
        assert_eq!(
            direct,
            at("s", &store(root.join("real").join(".").join("lambo.db"))),
            "a redundant spelling is the same store"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file that does not exist yet resolves through its parent, which is
    /// where a relative path's ambiguity lives. This is the common case on the
    /// serve path: `SqliteStore::connect` builds a **lazy** pool with
    /// `create_if_missing`, so the file is often not there when the endpoint is
    /// derived.
    #[test]
    fn a_store_file_that_does_not_exist_yet_still_resolves_through_its_parent() {
        let root = scratch("not-yet");
        std::fs::create_dir_all(&root).unwrap();
        let real = std::fs::canonicalize(&root).unwrap();
        assert_eq!(
            canonical_store_path(&root.join("lambo.db").to_string_lossy()),
            real.join("lambo.db").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A URI spelling is left verbatim: `canonicalize` cannot resolve it, and
    /// the `cwd.join` fallback would then make one store's identity depend on
    /// the cwd — J2-R1-2 from the other side.
    #[test]
    fn a_uri_store_spelling_is_not_canonicalized() {
        for uri in ["file:x?mode=rwc", "sqlite://data.db", "file:/abs/x.db"] {
            assert_eq!(canonical_store_path(uri), uri, "{uri} must be left alone");
        }
        assert_eq!(canonical_store_path(""), "");
    }

    /// J2-R1-3: the shared fallback carries the euid; `XDG_RUNTIME_DIR` does not,
    /// because it already does.
    #[test]
    fn the_shared_endpoint_directory_is_per_uid() {
        assert_eq!(
            endpoint_dir_from(Some("/run/user/501"), 501),
            PathBuf::from("/run/user/501/lambo"),
            "XDG_RUNTIME_DIR is already per-user; spending path bytes twice would eat the \
             sun_path headroom for nothing"
        );
        assert_eq!(
            endpoint_dir_from(None, 501),
            PathBuf::from("/tmp/lambo-501")
        );
        // Set-but-empty is not set.
        assert_eq!(
            endpoint_dir_from(Some(""), 501),
            PathBuf::from("/tmp/lambo-501")
        );
        assert_eq!(
            endpoint_dir_from(Some("/"), 501),
            PathBuf::from("/tmp/lambo-501")
        );
        // The property the whole finding is about: no two uids share a
        // directory on the shared base, so the first user cannot lock the rest
        // out.
        assert_ne!(endpoint_dir_from(None, 501), endpoint_dir_from(None, 502));
        // And the bare shared directory that caused the lockout is unreachable.
        for dir in [endpoint_dir_from(None, 0), endpoint_dir()] {
            assert_ne!(dir, PathBuf::from("/tmp/lambo"), "{}", dir.display());
        }
    }

    /// J2-L1, measured live: `cursor-agent` scrubs `TMPDIR` from its MCP child
    /// and `opencode` passes macOS's per-user `TMPDIR` through, so with the old
    /// three-rung scheme two client products derived two directories for one
    /// session on one store — and cross-client memory was silently absent on
    /// unmodified default wiring.
    ///
    /// `TMPDIR` is no longer in the scheme, so the *whole class* is gone: what
    /// a client does to that variable cannot move this address.
    #[test]
    fn what_a_client_does_to_tmpdir_cannot_move_the_endpoint() {
        // The two observed environments, reduced to their difference.
        let scrubbed = endpoint_dir_from(None, 501);
        let inherited = endpoint_dir_from(None, 501);
        assert_eq!(
            scrubbed, inherited,
            "TMPDIR is not an input any more, so the two products agree by construction"
        );
        assert_eq!(scrubbed, PathBuf::from("/tmp/lambo-501"));
        // And the full derivation agrees too, which is the property that matters
        // — one dialable address for one session on one store.
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        assert_eq!(
            SessionEndpoint::resolve_in(&scrubbed, "s", &store).unwrap(),
            SessionEndpoint::resolve_in(&inherited, "s", &store).unwrap()
        );
    }

    /// J2-R1-3: the mode check must be about the directory entry we created, not
    /// about wherever a pre-placed symlink points.
    ///
    /// `std::fs::metadata` follows symlinks, so `/tmp/lambo-<uid> → /tmp/theirs`
    /// with `/tmp/theirs` at 0700 passed the old gate and we bound a socket
    /// inside a directory someone else controls.
    ///
    /// The ownership half of the check is not reachable from in-process — faking
    /// a foreign-owned directory needs root — so it is one `!=` against
    /// `geteuid()` with no test. The symlink half is the one an attacker
    /// actually has, and it is pinned here.
    #[test]
    fn a_symlinked_endpoint_directory_is_refused_rather_than_followed() {
        let root = short_scratch("s");
        let target = root.join("t");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&target)
            .unwrap();
        let link = root.join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let err = SessionEndpoint::resolve_in(&link, "s", &store)
            .unwrap()
            .bind()
            .expect_err("a symlinked endpoint directory must be refused");
        assert!(
            err.to_string().contains("symbolic link"),
            "the refusal must say what it refused: {err}"
        );
        // J2-R1-9: these literals are `\`-continued, and a collapsed
        // continuation leaves the continuation indent INSIDE the string. `cargo
        // fmt` cannot see it and nothing else would.
        assert!(
            !err.to_string().contains("  "),
            "an operator message must not carry a collapsed continuation indent: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mode gate itself, which the old docstring over-credited but which is
    /// still real: a directory an attacker pre-created world-writable is refused
    /// rather than bound into.
    #[test]
    fn a_world_writable_endpoint_directory_is_refused() {
        let root = short_scratch("w");
        // `DirBuilder` honours the umask, so set the mode explicitly.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let err = SessionEndpoint::resolve_in(&root, "s", &store)
            .unwrap()
            .bind()
            .expect_err("a world-writable endpoint directory must be refused");
        assert!(
            err.to_string().contains("reachable by other users"),
            "the refusal must name the consequence: {err}"
        );
        // J2-R1-9: these literals are `\`-continued, and a collapsed
        // continuation leaves the continuation indent INSIDE the string. `cargo
        // fmt` cannot see it and nothing else would.
        assert!(
            !err.to_string().contains("  "),
            "an operator message must not carry a collapsed continuation indent: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
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
                SessionEndpoint::for_store("s", &store),
                None,
                "{store:?} is private to this process"
            );
        }
        // A real file, and a cluster, are both reachable by a second process.
        assert!(
            SessionEndpoint::for_store("s", &cfg(StoreKind::Sqlite, Some("/a.db"), None)).is_some()
        );
        assert!(SessionEndpoint::for_store(
            "s",
            &cfg(StoreKind::Cockroach, None, Some("postgres://h/db"))
        )
        .is_some());
    }

    /// **JE2E-2, the reviewer's own test shape.** Bind a holder, bind a second
    /// at the same address (which is what a lawful takeover does — the address
    /// is a pure function of session and store, so every generation lands here),
    /// then run the *first* one's exit path. The second's socket must survive.
    ///
    /// The failure this pins is not theoretical: the exit unlink was
    /// unconditional, so a fenced ex-holder resuming after a >45 s wedge deleted
    /// the live holder's socket, silently disabling multi-client attach for the
    /// new holder's whole lifetime — and every later loser was then told the
    /// holder "has most likely died", which was false.
    ///
    /// The socket is a real `UnixListener`, not a placed file: the identity that
    /// licenses the unlink is the inode `bind` created, and `bind`'s own
    /// stale-socket branch is what replaces it.
    #[tokio::test]
    async fn a_superseded_holders_exit_does_not_unlink_the_live_holders_socket() {
        let root = short_scratch("u");
        // `bind` creates its parent at 0700 and then insists on it; a scratch
        // directory made by the test would carry the umask's mode instead, so
        // the endpoint lives one rung down where `bind` owns the mode.
        let dir = root.join("l");
        let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
        let a = SessionEndpoint::resolve_in(&dir, "s", &store).unwrap();

        let a_listener = a.bind().expect("A binds");
        let a_bound = a.file_identity().expect("A's socket exists");

        // B takes the session and binds the same derived address: `bind` clears
        // A's now-stale socket under the lease's licence and creates its own.
        let b = SessionEndpoint::resolve_in(&dir, "s", &store).unwrap();
        assert_eq!(a.path(), b.path(), "every generation binds one address");
        drop(a_listener);
        let _b_listener = b.bind().expect("B binds after clearing the stale socket");
        let b_bound = b.file_identity().expect("B's socket exists");
        assert_ne!(
            a_bound, b_bound,
            "B's bind made a NEW inode — which is the fact the licence rests on"
        );

        // A's exit path runs, holding the endpoint it derived and the identity
        // it bound.
        a.unlink_if_ours(Some(a_bound));

        assert_eq!(
            b.file_identity(),
            Some(b_bound),
            "the live holder's socket must still be there, and must still be ITS socket"
        );

        // And the licence is not a blanket refusal: B's own exit does clean up
        // after itself, or every start would log a stale-socket warning it did
        // not earn.
        b.unlink_if_ours(Some(b_bound));
        assert_eq!(b.file_identity(), None, "a holder clears its own socket");

        let _ = std::fs::remove_dir_all(&root);
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
