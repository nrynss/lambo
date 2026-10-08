use super::*;

fn cfg(kind: StoreKind, path: Option<&str>, dsn: Option<&str>) -> StoreConfig {
    StoreConfig {
        kind,
        dsn: dsn.map(str::to_string),
        path: path.map(str::to_string),
        ..StoreConfig::default()
    }
}

/// A short, fixed endpoint directory, so these tests assert on the derivation
/// rather than on whatever `TMPDIR` this machine happens to have.
fn at(session: &str, store: &StoreConfig) -> SessionEndpoint {
    SessionEndpoint::resolve_in(Path::new("/run/lambo"), session, store).unwrap()
}

/// A scratch directory removed on drop, so a test that panics part-way
/// leaves nothing behind. Derefs to [`Path`].
struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A unique scratch directory for the tests that need real files — the
/// canonicalization ones do, since that is the whole point of them.
fn scratch(tag: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "lambo-endpoint-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

/// A scratch directory short enough to hold a socket address — the bind
/// tests need a real directory AND a path under [`SUN_PATH_MAX`], and macOS's
/// `TMPDIR` is 46 bytes before anything is joined to it.
fn short_scratch(tag: &str) -> crate::test_util::ScratchDir {
    crate::test_util::ScratchDir::short(tag)
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
    assert!(SessionEndpoint::for_store(
        "s",
        &cfg(StoreKind::Postgres, None, Some("postgres://h/db"))
    )
    .is_some());
}

/// B1 / J2-R1-2: a DSN is a spelling. Two spellings of one database must
/// derive one session endpoint, or two serves on one machine each believe
/// they are alone. Password is not in the identity string (so it cannot
/// reach the filesystem or the lease row).
#[test]
fn two_spellings_of_one_database_derive_one_endpoint() {
    let a = at(
        "s",
        &cfg(StoreKind::Postgres, None, Some("postgres://u@host/db")),
    );
    let b = at(
        "s",
        &cfg(StoreKind::Postgres, None, Some("postgres://u@host:5432/db")),
    );
    assert_eq!(
        a, b,
        "omitting the default port must not mint a second endpoint"
    );
    assert_eq!(
        a,
        at(
            "s",
            &cfg(
                StoreKind::Postgres,
                None,
                Some("postgresql://u@HOST/db?sslmode=require")
            )
        ),
        "postgresql://, host case, and sslmode are spelling, not identity"
    );
    let with_password = cfg(
        StoreKind::Postgres,
        None,
        Some("postgres://u:s3cret@host/db"),
    );
    assert_eq!(
        a,
        at("s", &with_password),
        "password is a credential, not the database"
    );
    assert_eq!(
        a,
        at(
            "s",
            &cfg(
                StoreKind::Postgres,
                None,
                Some("postgres://u:other@host:5432/db")
            )
        )
    );
    let ident = store_identity(&with_password);
    assert!(
        !ident.contains("s3cret"),
        "password must not appear in the pre-hash identity: {ident}"
    );
    assert!(
        !a.published().contains("s3cret"),
        "password must not appear in the lease-published path"
    );
    // Omitted database defaults to the explicit username (libpq). A DSN
    // without `/db` and the same DSN with `/u` are one database.
    let omitted_db = at(
        "s",
        &cfg(StoreKind::Postgres, None, Some("postgres://u@host")),
    );
    assert_eq!(
        omitted_db,
        at(
            "s",
            &cfg(StoreKind::Postgres, None, Some("postgres://u@host/u"))
        ),
        "omitting the database must not mint a second endpoint"
    );
    assert_ne!(
        omitted_db, a,
        "username-as-database is not the same store as an explicit /db"
    );
    // Libpq key=value is a spelling of the same database as the URL form.
    assert_eq!(
        a,
        at(
            "s",
            &cfg(
                StoreKind::Postgres,
                None,
                Some("host=host user=u dbname=db")
            )
        ),
        "libpq key=value must derive the same endpoint as the URL form"
    );
    // Different database, different endpoint.
    assert_ne!(
        a,
        at(
            "s",
            &cfg(StoreKind::Postgres, None, Some("postgres://u@host/other"))
        )
    );
    // Kind is part of identity: the same DSN on Cockroach is a different store.
    assert_ne!(
        a,
        at(
            "s",
            &cfg(StoreKind::Cockroach, None, Some("postgres://u@host/db"))
        )
    );
    // The same two spellings on Cockroach also collapse: the hazard is the
    // DSN, not the kind.
    assert_eq!(
        at(
            "s",
            &cfg(StoreKind::Cockroach, None, Some("postgres://u@host/db"))
        ),
        at(
            "s",
            &cfg(
                StoreKind::Cockroach,
                None,
                Some("postgres://u@host:5432/db")
            )
        )
    );
    assert_eq!(
        store_dsn_identity("postgres://u@host/db"),
        "postgres://u@host:5432/db"
    );
    assert_eq!(
        store_dsn_identity("postgres://u:s3cret@host/db"),
        "postgres://u@host:5432/db"
    );
    assert_eq!(
        store_dsn_identity("postgres://u@host"),
        "postgres://u@host:5432/u"
    );
    assert_eq!(
        store_dsn_identity("host=host user=u dbname=db"),
        "postgres://u@host:5432/db"
    );
}

/// B1-R1-2: shareable is ruled per kind, not defaulted. Collapsing
/// Cockroach and Postgres to `_ => true` (Sqlite still special-cased)
/// left the value pin green. This test names every variant and forbids
/// `_ =>` in the function body, so that mutation goes red.
#[test]
fn store_is_shareable_is_ruled_not_defaulted() {
    fn arm(kind: StoreKind) -> &'static str {
        match kind {
            StoreKind::Memory => "StoreKind::Memory =>",
            StoreKind::Cockroach => "StoreKind::Cockroach =>",
            StoreKind::Postgres => "StoreKind::Postgres =>",
            StoreKind::Sqlite => "StoreKind::Sqlite =>",
        }
    }
    let body = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mcp/endpoint.rs"))
        .split("fn store_is_shareable(store: &StoreConfig) -> bool {")
        .nth(1)
        .and_then(|s| s.split("\nfn ").next())
        .expect("store_is_shareable body");
    for kind in [
        StoreKind::Memory,
        StoreKind::Cockroach,
        StoreKind::Postgres,
        StoreKind::Sqlite,
    ] {
        let needle = arm(kind);
        assert!(
            body.contains(needle),
            "store_is_shareable must name {needle} (ruled, not defaulted)"
        );
    }
    assert!(
        !body.contains("_ =>"),
        "store_is_shareable must not default via `_ =>`; a future kind would inherit a ruling"
    );
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
}

/// **JE2E-R2-3.** The licence used to be `(dev, ino)`, and inode numbers are
/// recycled: ext4 allocates first-free, so a successor binding after this
/// process's inode is freed can be handed the same pair back — and a licence
/// checking only those two then "matches" a live successor's socket and
/// deletes it, which is JE2E-2's failure returning through a narrower door.
///
/// The recycle cannot be provoked on the developer filesystems this suite
/// runs on (APFS and tmpfs do not reuse inode numbers), so the property is
/// asserted where it lives instead: **an identity captured for one file does
/// not match a different file at the same path**, even when the first is
/// removed before the second is made — which is exactly the recycling
/// sequence, minus the allocator's cooperation. Under a `(dev, ino)`-only
/// licence this assertion is what a recycling filesystem would break.
#[tokio::test]
async fn a_recreated_socket_at_the_same_path_is_not_the_one_we_bound() {
    let root = short_scratch("r");
    let dir = root.join("l");
    let store = cfg(StoreKind::Sqlite, Some("/one.db"), None);
    let ep = SessionEndpoint::resolve_in(&dir, "s", &store).unwrap();

    let first = ep.bind().expect("first bind");
    let first_id = ep.file_identity().expect("first socket exists");

    // Free it, then make a new socket at the same path — the sequence an
    // inode-recycling allocator turns into "same (dev, ino)".
    drop(first);
    std::fs::remove_file(ep.path()).expect("free the inode");
    // `ctime` has second-and-nanosecond resolution; sleep past any tick
    // boundary so the assertion is about identity rather than timer luck.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let _second = ep.bind().expect("second bind at the same path");
    let second_id = ep.file_identity().expect("second socket exists");

    assert_ne!(
        first_id, second_id,
        "a recreated socket must not carry the identity of the one it replaced"
    );

    // **The recycle itself.** APFS and tmpfs never hand the same inode
    // number back, so the assertion above passes with or without the
    // `ctime` field and proves nothing about it. This is the opposing
    // input, built rather than provoked: the predecessor's identity as a
    // recycling allocator would have produced it — the *successor's*
    // `(dev, ino)`, wearing the *predecessor's* timestamp.
    //
    // Under a `(dev, ino)`-only licence this equals `second_id`, the
    // comparison "matches", and a live successor's socket is deleted by a
    // dead predecessor's exit — JE2E-2's failure returning through a
    // narrower door.
    let recycled = second_id.with_ctime_of(&first_id);
    assert_ne!(
        recycled, second_id,
        "the same inode number with an older change time is NOT the same file; a licence \
         that cannot tell these apart deletes live sockets on a recycling filesystem"
    );
    ep.unlink_if_ours(Some(recycled));
    assert_eq!(
        ep.file_identity(),
        Some(second_id),
        "a predecessor holding a recycled identity must not delete the live socket"
    );

    // And the plain case: the first holder's own exit leaves the second's
    // socket alone too.
    ep.unlink_if_ours(Some(first_id));
    assert_eq!(
        ep.file_identity(),
        Some(second_id),
        "the live socket survives a superseded holder's exit"
    );
}

#[test]
fn the_hash_is_pinned_so_a_compiler_upgrade_cannot_move_an_endpoint() {
    // FNV-1a 64 of the empty string is its offset basis, and of "a" the
    // basis times the prime. Pinned literally: this hash is baked into a
    // path two processes must agree on across builds.
    assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
}
