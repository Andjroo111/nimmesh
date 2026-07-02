//! G49: a deterministic smoke test for the swap browser demo's static serving
//! ([`nimmesh_core::demo_http`]). No port, no sockets — it calls `serve_static` directly, so the
//! G38/G46 intents demo can't silently rot and the path-traversal sandbox stays closed.

use nimmesh_core::demo_http::{
    health_fixture_json, intents_fixture_json, serve_static, stats_fixture_json,
};
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

#[test]
fn the_discovery_api_fixtures_are_well_formed_json() {
    // G54: the /api/intents + /api/stats fixture bodies the demo server serves carry the same data the
    // page shows. Marker-checked (no serde) — deterministic, no sockets.
    let intents = intents_fixture_json();
    assert!(
        intents.trim_start().starts_with('{'),
        "intents is a JSON object"
    );
    assert!(intents.contains("\"intents\""), "has an intents array");
    assert!(
        intents.contains("12 500") && intents.contains("\"NIM\"") && intents.contains("\"BTC\""),
        "carries the fixture rows + tickers"
    );
    assert!(
        intents.contains("\"fresh\":false"),
        "carries the expired row"
    );

    let stats = stats_fixture_json();
    assert!(
        stats.trim_start().starts_with('{'),
        "stats is a JSON object"
    );
    assert!(stats.contains("\"seen\":128"), "carries the seen count");
    assert!(
        stats.contains("\"matched\":12"),
        "carries the matched count"
    );
    assert!(
        stats.contains("\"throttled\":7"),
        "carries a dropped-by-reason count"
    );

    // G57: the health fixture is derived from the same counts, so it stays consistent with the stats.
    let health = health_fixture_json();
    assert!(
        health.trim_start().starts_with('{'),
        "health is a JSON object"
    );
    assert!(
        health.contains("\"status\":\"Healthy\""),
        "12 matches → Healthy"
    );
    assert!(
        health.contains("\"matchRatePct\":23"),
        "12 / (12 + 39 dropped) = 23%"
    );
    assert!(
        health.contains("\"dominantDrop\":\"rate\""),
        "the rate drop (18) is the largest"
    );
}
