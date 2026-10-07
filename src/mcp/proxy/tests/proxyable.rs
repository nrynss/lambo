//! Whether a lease holder can be proxied to: endpoint, host and address
//! checks.

use super::*;

fn row(holder: &str, endpoint: Option<&str>) -> LeaseInfo {
    LeaseInfo {
        holder: holder.to_string(),
        token: 1,
        acquired_at: Utc::now(),
        expires_at: Utc::now(),
        endpoint: endpoint.map(str::to_string),
    }
}

#[test]
fn a_holder_that_published_no_endpoint_is_not_proxyable() {
    let ours = ours();
    let err = proxyable(&row("a@this-host#1", None), &ours, "this-host").unwrap_err();
    assert_eq!(err, NotProxyable::HolderPublishedNoEndpoint);
    // The explanation must name the common cause — a CLI verb holding the
    // lease briefly — because "wait" is the right action and "debug your
    // socket" is not.
    assert!(err.explain().contains("CLI verb"), "{}", err.explain());
}

#[test]
fn a_holder_on_another_host_is_not_proxyable() {
    let ours = ours();
    let err = proxyable(
        &row("a@other-host#1", Some(&ours.published())),
        &ours,
        "this-host",
    )
    .unwrap_err();
    assert_eq!(
        err,
        NotProxyable::HolderIsOnAnotherHost {
            holder: "a@other-host#1".into()
        }
    );
    // A socket path is meaningless off-host even when it happens to match,
    // so the host is checked BEFORE the path — this row's path is ours.
    assert!(err.explain().contains("other-host"));
}

#[test]
fn an_endpoint_this_build_does_not_derive_is_not_proxyable() {
    let ours = ours();
    let err = proxyable(
        &row("a@this-host#1", Some("/run/somewhere-else/x.sock")),
        &ours,
        "this-host",
    )
    .unwrap_err();
    assert!(matches!(err, NotProxyable::EndpointIsNotOurs { .. }));
    assert!(
        err.explain().contains("different graph"),
        "the refusal must say what forwarding anyway would risk: {}",
        err.explain()
    );
}

#[test]
fn a_local_holder_publishing_our_endpoint_is_proxyable() {
    let ours = ours();
    assert_eq!(
        proxyable(
            &row("a@this-host#4213", Some(&ours.published())),
            &ours,
            "this-host"
        )
        .expect("our own derivation is proxyable"),
        ours.path().to_path_buf(),
        "the address to dial is the published one, which here equals ours"
    );
}

/// J2-L1, measured live: `cursor-agent` scrubs `TMPDIR` from the environment
/// of the MCP server it spawns and `opencode` passes macOS's per-user
/// `TMPDIR` through, so before this the two products' serves derived two
/// **directories** for one session on one store and the loser refused to
/// forward — cross-client memory silently absent on default wiring.
///
/// The directory may differ. The **name** may not: it is a hash of the
/// session and the canonicalized store identity, so a match means the same
/// session on the same store and the directory decides only reachability.
#[test]
fn a_holder_publishing_the_same_address_in_another_directory_is_proxyable() {
    let ours = ours();
    let name = ours
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    // The two directories the live probe actually produced, in shape.
    for dir in [
        "/var/folders/q1/4dwfdvt563ng8lwybj8bry_c0000gn/T/lambo-501",
        "/run/user/501/lambo",
    ] {
        let published = format!("{dir}/{name}");
        let address = proxyable(&row("a@this-host#1", Some(&published)), &ours, "this-host")
            .unwrap_or_else(|e| {
                panic!("a matching address name must be proxyable: {}", e.explain())
            });
        assert_eq!(
            address,
            std::path::PathBuf::from(&published),
            "the address to DIAL is the holder's published path, not this process's \
                 derivation — that is the whole fix"
        );
        assert_ne!(
            address,
            ours.path(),
            "and this test is only meaningful because the two differ"
        );
    }
}

/// The other side of the same decision: a differing **name** is the real
/// different-graph case and stays a refusal, because the name is the only
/// thing carrying identity.
#[test]
fn a_holder_publishing_a_different_address_name_is_not_proxyable() {
    let ours = ours();
    let dir = ours.path().parent().unwrap().display().to_string();
    for published in [
        // Same directory, another session or another store.
        format!("{dir}/other-0000000000000000.sock"),
        // Our own name with the hash altered by one nibble.
        {
            let name = ours
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            format!("{dir}/{}", name.replacen(".sock", "x.sock", 1))
        },
        // No name at all.
        "/".to_string(),
        String::new(),
        // J2-R2-6: the RIGHT name, published relatively. `dial_dir` would
        // have taken `parent()` — `Some("")` for the bare spelling, which
        // reached `assert_private_dir` as an empty path in an
        // operator-facing message, and `.` for the other, which is this
        // process's cwd rather than anywhere on the holder's filesystem.
        ours.path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        format!("./{}", ours.path().file_name().unwrap().to_string_lossy()),
    ] {
        let err = proxyable(&row("a@this-host#1", Some(&published)), &ours, "this-host")
            .expect_err("a different address name must be refused");
        assert!(
            matches!(err, NotProxyable::EndpointIsNotOurs { .. }),
            "{published:?} gave {err:?}"
        );
    }
}

/// J1 takes `agent_id` untrimmed and unnormalised, so an agent may name
/// itself `weird@host#9`. The host is the segment between the LAST `@` and
/// the last `#`, exactly as `LeaseHolder::token` composes it — otherwise a
/// self-chosen id could make a remote holder look local.
#[test]
fn an_agent_id_containing_at_and_hash_does_not_confuse_the_host_check() {
    assert!(holder_is_on_host("a@b#c@this-host#4213", "this-host"));
    assert!(!holder_is_on_host(
        "a@this-host#1@other-host#2",
        "this-host"
    ));
    // A malformed token names no host and must not pass.
    assert!(!holder_is_on_host("no-at-sign", "this-host"));
}
