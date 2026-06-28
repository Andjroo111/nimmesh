//! # swap_demo_server — the browser demo backed by the REAL engine (sim chain)
//!
//! A tiny std-only HTTP server (no extra deps) that runs a [`nimmesh_core::swap_sim::SwapSim`] — a
//! full initiator+responder NIM⇄BTC atomic swap on the real [`SwapEngine`] against an in-memory sim
//! chain — and serves the `webui/` so the browser UI drives it. The UI's phases, locks, and tx ids
//! come from the **real engine**, not a mock.
//!
//! Run from the repo root:
//! ```sh
//! cargo run --example swap_demo_server --features bitcoin-leg
//! # → open http://127.0.0.1:8090/swap/swap.html?engine=1
//! ```
//! Env: `PORT` (default 8090), `NIMMESH_WEBUI` (default `webui`).
//! API: `GET /api/state`, `POST /api/advance`, `POST /api/reset`. **Sim/testnet only; no real funds.**

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nimmesh_core::swap_sim::{SwapSim, SwapSnapshot};

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8090);
    let webui = std::env::var("NIMMESH_WEBUI").unwrap_or_else(|_| "webui".to_string());
    let webui_root = std::fs::canonicalize(&webui).unwrap_or_else(|_| {
        panic!("webui dir not found at '{webui}' (run from the repo root, or set NIMMESH_WEBUI)")
    });
    let sim = Mutex::new(SwapSim::new());

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!("nimmesh swap demo — REAL engine, sim chain (no real funds)");
    println!("  open  http://127.0.0.1:{port}/swap/swap.html?engine=1");
    for stream in listener.incoming().flatten() {
        handle(stream, &sim, &webui_root);
    }
}

fn handle(mut stream: TcpStream, sim: &Mutex<SwapSim>, webui_root: &Path) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // drain headers
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");

    let (status, ctype, body) = route(method, path, sim, webui_root);
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
}

fn route(
    method: &str,
    path: &str,
    sim: &Mutex<SwapSim>,
    webui_root: &Path,
) -> (&'static str, &'static str, Vec<u8>) {
    match (method, path) {
        ("POST", "/api/advance") => {
            let mut s = sim.lock().unwrap();
            let body = match s.step() {
                Ok(snap) => snap_json(&snap),
                Err(e) => format!("{{\"error\":\"{}\"}}", escape(&e.to_string())),
            };
            ("200 OK", "application/json", body.into_bytes())
        }
        ("POST", "/api/reset") => {
            let mut s = sim.lock().unwrap();
            s.reset();
            (
                "200 OK",
                "application/json",
                snap_json(&s.snapshot()).into_bytes(),
            )
        }
        ("POST", "/api/refund") => {
            // The safety net: stall the swap and reclaim both legs via the timeout refund.
            let mut s = sim.lock().unwrap();
            let body = match s.stall_and_refund() {
                Ok(snap) => snap_json(&snap),
                Err(e) => format!("{{\"error\":\"{}\"}}", escape(&e.to_string())),
            };
            ("200 OK", "application/json", body.into_bytes())
        }
        ("GET", "/api/state") => {
            let s = sim.lock().unwrap();
            (
                "200 OK",
                "application/json",
                snap_json(&s.snapshot()).into_bytes(),
            )
        }
        ("GET", _) => serve_static(path, webui_root),
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    }
}

/// Serve a file from `webui_root`, sandboxed (no path traversal). `/` → the swap page.
fn serve_static(path: &str, webui_root: &Path) -> (&'static str, &'static str, Vec<u8>) {
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

fn content_type(p: &Path) -> &'static str {
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

fn snap_json(s: &SwapSnapshot) -> String {
    let tx = match &s.last_tx_id {
        Some(t) => format!("\"{}\"", escape(t)),
        None => "null".to_string(),
    };
    format!(
        "{{\"step\":{},\"total\":{},\"label\":\"{}\",\"initiatorPhase\":\"{}\",\"responderPhase\":\"{}\",\"nimLocked\":{},\"btcLocked\":{},\"secretRevealed\":{},\"refunded\":{},\"lastTxId\":{},\"btcHtlcAddress\":\"{}\",\"done\":{}}}",
        s.step,
        s.total,
        escape(&s.label),
        escape(&s.initiator_phase),
        escape(&s.responder_phase),
        s.nim_locked,
        s.btc_locked,
        s.secret_revealed,
        s.refunded,
        tx,
        escape(&s.btc_htlc_address),
        s.done,
    )
}

/// Minimal JSON string escaping (the values here are controlled, but be safe).
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
