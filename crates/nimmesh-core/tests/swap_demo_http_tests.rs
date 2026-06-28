//! G49: a deterministic smoke test for the swap browser demo's static serving
//! ([`nimmesh_core::demo_http`]). No port, no sockets — it calls `serve_static` directly, so the
//! G38/G46 intents demo can't silently rot and the path-traversal sandbox stays closed.

use nimmesh_core::demo_http::serve_static;
use std::path::PathBuf;

/// The demo tree, resolved from the crate manifest dir so the test is CWD-independent. `serve_static`
/// needs a canonical root (the example canonicalizes it at startup), so we canonicalize here too.
fn webui_root() -> PathBuf {
    std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/../../webui"))
        .expect("webui dir resolves from the crate manifest dir")
}

#[test]
fn the_intents_demo_and_its_assets_serve() {
    let root = webui_root();

    let (status, ctype, body) = serve_static("/swap/intents.html", &root);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "text/html; charset=utf-8");
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Open intents"), "intents.html lost its title");
    assert!(
        html.contains("Discovery this session"),
        "intents.html lost the G46 discovery-stats strip"
    );

    let (status, ctype, _) = serve_static("/swap/intents.css", &root);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "text/css; charset=utf-8");

    let (status, _, _) = serve_static("/swap/swap.html", &root);
    assert_eq!(status, "200 OK");

    // `/` maps to the swap page.
    let (status, ctype, _) = serve_static("/", &root);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "text/html; charset=utf-8");

    // The vendored @nimiq/iqons sprite the identicons render from.
    let (status, ctype, _) = serve_static("/nimiq/assets/img/iqons.min.svg", &root);
    assert_eq!(status, "200 OK");
    assert_eq!(ctype, "image/svg+xml");
}

#[test]
fn unknown_paths_404_and_traversal_is_refused() {
    let root = webui_root();

    let (status, _, _) = serve_static("/swap/does-not-exist.html", &root);
    assert_eq!(status, "404 Not Found");

    // Every `..` traversal is refused — it must never reach a file outside the webui root.
    for evil in [
        "/../Cargo.toml",
        "/swap/../../Cargo.toml",
        "/../../Cargo.toml",
    ] {
        let (status, _, _) = serve_static(evil, &root);
        assert_ne!(
            status, "200 OK",
            "traversal `{evil}` must not escape the webui root"
        );
    }
}
