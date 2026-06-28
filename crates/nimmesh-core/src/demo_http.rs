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
