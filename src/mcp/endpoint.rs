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
//! the file half of the identity is canonicalized before it is hashed. The DSN
//! half is normalised the same way (B1): omitted port is 5432, host is
//! lowercased, connection parameters that do not name the database are dropped,
//! and the password is stripped so it is not even in the pre-hash string.
//!
//! The published value is still read from the row rather than assumed, because
//! the row is the authority on where *this* holder listens: a proxy compares the
//! row against its own derivation and refuses honestly when they disagree,
//! rather than dialling a path the holder never bound.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::store::dsn::store_dsn_identity;
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

/// What identifies the socket file a holder bound
/// ([`SessionEndpoint::file_identity`]).
///
/// `(device, inode)` plus the inode's **change time**, and the third field is
/// not decoration (JE2E-R2-3): `(dev, ino)` alone recycles. On an
/// inode-recycling filesystem — ext4 allocates first-free — a successor that
/// binds after this process's inode has been freed can be handed the *same*
/// `(dev, ino)`, and a licence checking only those two would then "match" a live
/// successor's socket and delete it. A recreated inode gets a fresh `ctime`, so
/// all three together do not recycle.
///
/// **`ctime` rather than birth time**, deliberately: `st_birthtime` is absent on
/// some Linux filesystems and reaches Rust as a fallible `Metadata::created()`,
/// so an identity built on it is `Option`-shaped exactly where the guarantee is
/// wanted. `ctime` is on every unix `stat`, is set at creation, and is stable
/// for this socket's life — nothing re-permissions the file after
/// [`SessionEndpoint::bind`]'s one `set_permissions`, which runs before the
/// capture.
///
/// Every way this comparison can be wrong fails toward **not deleting**: a
/// spurious mismatch leaves a socket for the next holder's `bind` to clear under
/// the lease's licence, which is the mechanism that existed before any of this
/// and still backstops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketIdentity {
    dev: u64,
    ino: u64,
    ctime: i64,
    ctime_nsec: i64,
}

impl SocketIdentity {
    /// This identity's `(dev, ino)` carrying `other`'s change time — **what an
    /// inode-recycling filesystem hands back**, built directly because no
    /// filesystem this suite runs on will produce it.
    ///
    /// APFS and tmpfs (the two default endpoint directories) do not reuse inode
    /// numbers, so a test that merely creates, removes and re-creates a socket
    /// gets two distinct inodes and passes with or without the `ctime` field —
    /// it would be a fixture that cannot fail, which is the defect this
    /// remediation round was already convicted of once (JE2E-R2-7). This
    /// constructor is the opposing input: the successor's inode wearing the
    /// predecessor's timestamp, which is precisely the collision `(dev, ino)`
    /// alone cannot see.
    #[cfg(test)]
    pub(crate) fn with_ctime_of(&self, other: &Self) -> Self {
        Self {
            dev: self.dev,
            ino: self.ino,
            ctime: other.ctime,
            ctime_nsec: other.ctime_nsec,
        }
    }
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

    /// [`SessionEndpoint::for_store`] with the endpoint directory supplied, for the
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
    /// Fails only when the derived path cannot fit `SUN_PATH_MAX`, which is a
    /// property of the *endpoint directory*, not of the session name — the name's
    /// contribution is bounded by construction. The message therefore points at
    /// the thing the operator can change. [`SessionEndpoint::for_store`] turns
    /// that failure into `None` rather than a refused start (J2-R1-5); this
    /// function still reports it, because a caller that wants the reason should
    /// be able to have it.
    pub fn resolve(session: &str, store: &StoreConfig) -> Result<Self, LamboError> {
        Self::resolve_in(&endpoint_dir(), session, store)
    }

    /// [`SessionEndpoint::resolve`] with the endpoint directory supplied.
    ///
    /// Split out so the derivation is testable without touching process-global
    /// environment: `set_var` is shared by every test in the binary, and two
    /// tests racing on `XDG_RUNTIME_DIR` is exactly the kind of flake that reads
    /// as a real defect on a loaded CI runner.
    ///
    /// `dir` is the endpoint directory itself (what the process would derive
    /// from `$XDG_RUNTIME_DIR`, i.e. `$XDG_RUNTIME_DIR/lambo`), not the runtime
    /// base above it. Returns the address a holder of `session` on `store`
    /// would bind inside that directory, without creating or checking the
    /// directory. Refuses, as [`SessionEndpoint::resolve`] does, when the
    /// resulting path would not fit a unix socket address (`SUN_PATH_MAX`).
    /// Use it to derive the address for an endpoint directory other than this
    /// process's own.
    pub fn resolve_in(dir: &Path, session: &str, store: &StoreConfig) -> Result<Self, LamboError> {
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
                 name is not the problem — its contribution is bounded — the endpoint directory is. \
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
    /// or other. Together with the per-uid name (see `endpoint_dir`) that is
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

    /// The identity of the socket file at this path right now, or `None` if
    /// nothing is there.
    ///
    /// Taken immediately after a successful [`SessionEndpoint::bind`] and
    /// handed back to [`SessionEndpoint::unlink_if_ours`] at exit — see there
    /// for why the *path* is not enough to identify our own socket, and for the
    /// one window this cannot close.
    pub fn file_identity(&self) -> Option<SocketIdentity> {
        std::fs::symlink_metadata(&self.path)
            .ok()
            .map(|m| SocketIdentity {
                dev: m.dev(),
                ino: m.ino(),
                ctime: m.ctime(),
                ctime_nsec: m.ctime_nsec(),
            })
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
    /// So the licence is identity, not authority: `bound` is the
    /// [`SocketIdentity`] this process saw the instant after it bound, and the
    /// file is removed only while the path still resolves to it. A new holder's
    /// `bind` unlinks the old file and creates a new one, and the fresh inode
    /// carries a fresh `ctime`, so a superseded endpoint's identity does not
    /// match a live successor's socket — including on a filesystem that recycles
    /// inode numbers, which `(dev, ino)` alone did not survive (JE2E-R2-3).
    ///
    /// # The window this cannot close, named rather than claimed away
    ///
    /// This said "can never match … by construction". It is not *by
    /// construction*, and the residual is worth stating precisely, because it is
    /// inherent to reading an identity through a path:
    ///
    /// **The capture instant.** `bound` comes from a `stat` of the path taken
    /// just after `bind` returns, not from the listener itself. If this process
    /// were descheduled between those two steps for longer than a lease TTL —
    /// long enough for a successor to fence it, win the lapsed lease and bind
    /// its own socket — the `stat` would capture the *successor's* identity as
    /// ours, and the exit would then match and delete it. `fstat` on the
    /// listening fd cannot help: for a unix socket it reports the socket
    /// object's inode, not the filesystem inode of the path, so it is not
    /// comparable with what the exit can see. The window is a few microseconds
    /// of ordinary scheduling against a 45-second precondition.
    ///
    /// What is left after that is bounded by the backstop that predates all of
    /// this: a socket wrongly removed is re-created by the next holder's `bind`,
    /// and the cost is the multi-client attach of one holder generation — the
    /// original JE2E-2 failure, at a probability the fix moved from "any wedged
    /// predecessor's exit" to "a wedge landing inside two adjacent statements".
    ///
    /// `None` means the bind never happened (or the stat failed), and nothing is
    /// removed: this process put no file here to clean up.
    pub fn unlink_if_ours(&self, bound: Option<SocketIdentity>) {
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
///
/// The first rung (`<base>/lambo`) is mirrored by `RuntimeDir::derived_endpoint_dir`
/// in `tests/common/mod.rs`, which cannot call this private function; change
/// one and update the other.
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
/// The DSN half is **normalised**, not taken verbatim: a DSN is a spelling and
/// this function must produce an identity; see [`store_dsn_identity`]. The
/// path half is **canonicalized** first for the same reason; see
/// [`canonical_store_path`] and J2-R1-2. The password never appears in the
/// string that is hashed, so it cannot reach the filesystem or the lease row
/// even if hashing were skipped. That holds on the unparseable path too, and
/// only because of how: a spelling `store_dsn_identity` cannot account for is
/// replaced by a SHA-256 digest of itself rather than echoed (B-E2E-R3-1, and
/// B-E2E-R4-2 for why the digest and not a constant). Between R2-5 and R3-1
/// this sentence was false for a DSN with userinfo and no `://`.
///
/// The digest also means two malformed spellings are two identities, so two
/// serves whose DSNs this module cannot parse do not share a socket path unless
/// they wrote the same string. Round 3's constant made every one of them share
/// one, which is the J2-R1-2 collision in a different costume.
fn store_identity(store: &StoreConfig) -> String {
    format!(
        "{:?}\u{1f}{}\u{1f}{}",
        store.kind,
        store_dsn_identity(store.dsn.as_deref().unwrap_or("")),
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
        // Networked store another process can open: the same ruling as
        // Cockroach. Ruled here, not defaulted, because the match is
        // exhaustive (J2, B1-forced).
        StoreKind::Postgres => true,
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
mod tests;
