//! # demo_http — the static-file serving behind the swap browser demo (G49)
//!
//! Extracted from the `swap_demo_server` example so the static-serving — and its path-traversal
//! sandbox — is covered by a deterministic test, and the G38/G46 intents demo can't silently rot.
//! Pure std: no `bitcoin-leg`, no sockets, no shared state. The example wires this into its tiny
//! HTTP loop; the test calls [`serve_static`] directly.

use std::path::{Path, PathBuf};

/// Serve a file from `webui_root`, sandboxed against path traversal. `/` maps to the swap page; any
/// `..` segment is refused, and the canonical resolved path must stay inside `webui_root` (so symlinks
/// or `.`-tricks can't escape). Returns `(status, content_type, body)`.
///
/// `webui_root` MUST already be canonical (the example canonicalizes it at startup) — the inside-root
/// check compares against it verbatim.
pub fn serve_static(path: &str, webui_root: &Path) -> (&'static str, &'static str, Vec<u8>) {
    let rel = if path == "/" {
        "swap/swap.html"
    } else {
        path.trim_start_matches('/')
    };
    if rel.split('/').any(|seg| seg == "..") {
        return ("403 Forbidden", "text/plain", b"forbidden".to_vec());
    }
    let full: PathBuf = webui_root.join(rel);
    match std::fs::canonicalize(&full) {
        Ok(p) if p.starts_with(webui_root) => match std::fs::read(&p) {
            Ok(bytes) => ("200 OK", content_type(&p), bytes),
            Err(_) => ("404 Not Found", "text/plain", b"not found".to_vec()),
        },
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    }
}

/// The MIME type for a path's extension (the small set the demo serves).
pub fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// G54: the demo's "open intents on the mesh" fixture, as JSON — the SAME four advertisements
/// `webui/swap/intents.html` renders. A fixture step toward OG-1: the live app streams the node's real
/// `SwapSession` intents across the native bridge, replacing this canned body. Hand-built, no serde.
pub fn intents_fixture_json() -> String {
    r#"{"head":4500000,"intents":[
{"gives":"NIM","giveAmount":"12 500","takeAsset":"BTC","takeAmount":"0.05","rate":"1 BTC = 250 000 NIM","fresh":true,"expiry":"in 3 000 blocks","peer":"NQ34 248J PA4S 8VK2 SS54 9N0B M89U HE5L AB48"},
{"gives":"BTC","giveAmount":"0.02","takeAsset":"NIM","takeAmount":"5 200","rate":"1 BTC = 260 000 NIM","fresh":true,"expiry":"in 2 200 blocks","peer":"NQ91 8E7L K0ST 3FM2 9R6D 7B1V XQH4 5N2P GJ0A"},
{"gives":"NIM","giveAmount":"40 000","takeAsset":"BTC","takeAmount":"0.16","rate":"1 BTC = 250 000 NIM","fresh":false,"expiry":"at block 4 499 000","peer":"NQ05 6RT2 9LMD 0KX7 V3B8 EH4N PS1Q 7J6F 2A0C"},
{"gives":"BTC","giveAmount":"0.1","takeAsset":"NIM","takeAmount":"26 000","rate":"1 BTC = 260 000 NIM","fresh":true,"expiry":"in 12 000 blocks","peer":"NQ77 1KD9 4VR0 8XPS 2M5H 6T3B EL9N 0AQ7 J84U"}
]}"#
    .to_string()
}

/// G54: the demo's discovery-stats fixture (the G42 `IntentMetrics` counters), as JSON — the SAME
/// numbers the intents view's "Discovery this session" strip shows. Live app: the node's real counters.
pub fn stats_fixture_json() -> String {
    r#"{"seen":128,"matched":12,"readvertised":34,"expired":9,"rate":18,"forged":5,"throttled":7}"#
        .to_string()
}
