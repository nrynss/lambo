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
    assert!(js.body.contains("/api/pulse"), "script body: {}", js.body);

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
    let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/serve_web.rs"));
    let prod = src.split("#[cfg(all(test").next().unwrap_or(src);
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
    let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/serve_web.rs"));
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
