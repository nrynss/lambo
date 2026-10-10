//! The router: the page and its assets, and the read-only guarantee.

use super::*;

// ---- (a) the router serves the page and the JSON endpoints ----------

#[tokio::test]
async fn serves_the_page_and_its_embedded_assets() {
    let store = Arc::new(MemoryStore::new());
    let (addr, handle) = spawn(state_on(store, "t85-assets")).await;

    let page = request(addr, "GET", "/").await;
    assert_eq!(page.status, 200);
    assert!(
        page.headers
            .to_lowercase()
            .contains("content-type: text/html"),
        "{}",
        page.headers
    );
    assert!(page.body.contains("<title>Lambo"), "{}", page.body);
    assert!(
        page.body.contains("/app.css") && page.body.contains("/app.js"),
        "the page must reference the embedded assets, not a CDN"
    );

    let css = request(addr, "GET", "/app.css").await;
    assert_eq!(css.status, 200);
    assert!(css.body.contains("--bg:"), "stylesheet body: {}", css.body);

    let js = request(addr, "GET", "/app.js").await;
    assert_eq!(js.status, 200);
    assert!(js.body.contains("\"api/pulse"), "script body: {}", js.body);

    let health = request(addr, "GET", "/healthz").await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body, "ok");

    handle.abort();
}

/// No asset may reach off-host: a stripped AWS task has no egress, and the
/// demo URL must not depend on a third party being up.
#[test]
fn embedded_assets_reference_no_external_origin() {
    for (name, body) in [
        ("index.html", INDEX_HTML),
        ("app.css", APP_CSS),
        ("app.js", APP_JS),
    ] {
        for needle in ["http://", "https://", "//cdn", "@import url("] {
            assert!(
                !body.contains(needle),
                "{name} references an external origin ('{needle}') — assets must be self-contained"
            );
        }
    }
}

// ---- (c) read-only guarantee ---------------------------------------

#[tokio::test]
async fn read_only_router_has_no_mutating_route() {
    let store = seed("t85-readonly").await;
    let (addr, handle) = spawn(state_on(store, "t85-readonly")).await;

    for path in ROUTES {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let r = request(addr, method, path).await;
            assert_eq!(
                r.status, 405,
                "{method} {path} must be Method Not Allowed — this is a read-only window \
                     and a mutating route here would be a stranger with a pen. Got {} / {}",
                r.status, r.body
            );
        }
    }

    handle.abort();
}

/// Source-level backstop for the behavioural test above: catches a mutating
/// route registered on a path that was also left out of [`ROUTES`].
#[test]
fn the_module_registers_only_get_routes() {
    let prod = production_source();
    let prod = prod.as_str();
    for banned in [
        "routing::post",
        "routing::put",
        "routing::patch",
        "routing::delete",
        "post(",
        "put(",
        "patch(",
        "delete(",
    ] {
        assert!(
            !prod.contains(banned),
            "serve_web registers '{banned}' — the demo app must stay read-only on every \
                 bind, token-protected or not"
        );
    }
    // A reader never opens a writer, never takes the lease, never spawns GC.
    for banned in [
        "Memory::builder",
        "open_writer",
        "acquire_lease",
        ".spawn()",
    ] {
        assert!(
            !prod.contains(banned),
            "serve_web contains '{banned}' — serve-web is a lease-free reader (spec §2.2)"
        );
    }
}

/// Every route the router answers is listed in [`ROUTES`], so the method
/// sweep above cannot silently miss one.
#[test]
fn routes_constant_covers_every_registered_route() {
    let src = router_source();
    let body = src
        .split("fn router(")
        .nth(1)
        .and_then(|s| s.split("\n}\n").next())
        .expect("router body");
    let registered: Vec<String> = body
        .match_indices(".route(\"")
        .filter_map(|(i, _)| {
            let rest = &body[i + ".route(\"".len()..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .collect();
    assert!(!registered.is_empty(), "parsed no routes from the router");
    for path in &registered {
        assert!(
            ROUTES.contains(&path.as_str()),
            "route '{path}' is not in ROUTES — the read-only method sweep would skip it"
        );
    }
    assert_eq!(registered.len(), ROUTES.len(), "ROUTES has stale entries");
}

/// #28: the source scans above read [`PRODUCTION_SOURCES`], and a portal
/// source missing from it would be scanned by nothing. Every `.rs` file
/// under `src/cli/serve_web/`, at any depth (the `serve_web/tests/`
/// directory aside), and the root must be listed. The walk recurses so a
/// further split into a subdirectory (`serve_web/routes/graph.rs`) cannot
/// slip past the scans.
#[test]
fn the_source_scans_cover_every_production_file() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli");
    let mut on_disk = vec!["serve_web.rs".to_string()];
    let mut pending = vec![("serve_web".to_string(), root.join("serve_web"))];
    while let Some((rel, dir)) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {rel}: {e}")) {
            let path = entry.expect("dir entry").path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let child = format!("{rel}/{name}");
            if path.is_dir() {
                if child != "serve_web/tests" {
                    pending.push((child, path));
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                on_disk.push(child);
            }
        }
    }
    on_disk.sort();
    let mut listed: Vec<String> = PRODUCTION_SOURCES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    listed.sort();
    assert_eq!(
        listed, on_disk,
        "PRODUCTION_SOURCES must list every portal production file, so the read-only and \
         route-coverage scans see all of them"
    );
}
