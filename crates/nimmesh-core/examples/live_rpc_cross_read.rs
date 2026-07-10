//! # live_rpc_cross_read — prove the M5 verifier cross-read against REAL testnet infra
//!
//! The G8 M5 hardening only trusts a funding depth when an INDEPENDENT second RPC endpoint agrees
//! on `head` (within [`HEAD_CROSS_TOLERANCE_BLOCKS`]); the conservative (lower) head then drives
//! the depth, so a single compromised/MITM'd endpoint can neither inflate depth nor fake "funded +
//! deep". This example exercises that EXACT comparison against real public endpoints — **read-only,
//! no key, no funds, no transaction** — so honest infra is shown to pass the agreement gate.
//!
//! - **Amoy:** two GENUINELY independent public endpoints (polygon.technology + publicnode) — the
//!   real cross-read the [`AmoyHtlcSwapVerifier`]/[`PolygonHtlcVerifier`] `with_secondary` wiring
//!   performs on the live money path.
//! - **NIM testnet:** the one public endpoint (`rpc.testnet.nimiqwatch.com`). A second INDEPENDENT
//!   testnet RPC is a self-hosted node (an M5/mainnet-gating item, see ADR-0011), so the NIM leg
//!   reads the single head and names the gap rather than pretending to cross-check.
//!
//! **TESTNET ONLY.** Run:
//! `cargo run --example live_rpc_cross_read --features polygon-gateway,gateway-rpc`
//! Optional env overrides: `AMOY_RPC_URL` · `AMOY_RPC_URL_2` · `NIM_TESTNET_RPC_URL`.

use std::error::Error;

use nimmesh_core::polygon_gateway::{HttpPolygonRpc, DEFAULT_AMOY_RPC_URL};
use nimmesh_core::polygon_verifier::HEAD_CROSS_TOLERANCE_BLOCKS;
use nimmesh_core::rpc::{GatewayRpc, HttpGatewayRpc, DEFAULT_TESTNET_RPC_URL};
use nimmesh_core::NetworkId;

/// A second, genuinely independent public Amoy endpoint (a different operator than the default).
const DEFAULT_AMOY_RPC_URL_2: &str = "https://polygon-amoy-bor-rpc.publicnode.com";

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn main() -> Result<(), Box<dyn Error>> {
    println!(
        "live_rpc_cross_read — M5 head cross-read vs REAL testnet infra (read-only, no funds)\n"
    );

    // ── Amoy: two independent endpoints, the real verifier cross-read ────────────────────────────
    let a1 = env("AMOY_RPC_URL").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL.into());
    let a2 = env("AMOY_RPC_URL_2").unwrap_or_else(|| DEFAULT_AMOY_RPC_URL_2.into());
    if a1 == a2 {
        return Err("the two Amoy endpoints must differ for a genuine cross-read".into());
    }
    let p1 = HttpPolygonRpc::new(a1.clone()).map_err(|e| format!("{e:?}"))?;
    let p2 = HttpPolygonRpc::new(a2.clone()).map_err(|e| format!("{e:?}"))?;
    let h1 = p1.block_number().map_err(|e| format!("{e:?}"))?;
    let h2 = p2.block_number().map_err(|e| format!("{e:?}"))?;
    let diff = h1.abs_diff(h2);
    println!("Amoy primary    {a1}\n     head = {h1}");
    println!("Amoy secondary  {a2}\n     head = {h2}");
    println!(
        "  |Δ head| = {diff}  (tolerance {HEAD_CROSS_TOLERANCE_BLOCKS} blocks) · conservative head = {}",
        h1.min(h2)
    );
    if diff > HEAD_CROSS_TOLERANCE_BLOCKS {
        return Err(format!(
            "Amoy endpoints disagree by {diff} > {HEAD_CROSS_TOLERANCE_BLOCKS} — the verifier would fail CLOSED here"
        )
        .into());
    }
    println!("  OK — Amoy cross-read AGREES; the verifier trusts the conservative depth.\n");

    // ── NIM testnet: single public endpoint (a second is a self-hosted node — gated) ─────────────
    let n1 = env("NIM_TESTNET_RPC_URL").unwrap_or_else(|| DEFAULT_TESTNET_RPC_URL.into());
    let nim = HttpGatewayRpc::new(n1.clone(), NetworkId::Testnet).map_err(|e| format!("{e:?}"))?;
    let nh = nim.block_number().map_err(|e| format!("{e:?}"))?;
    println!("NIM testnet     {n1}\n     head = {nh}");
    println!("  (single public endpoint — a second INDEPENDENT testnet RPC is a self-hosted node,");
    println!("   an M5/mainnet-gating item; wire it via NimHtlcVerifier::with_secondary when available.)\n");

    println!("M5 live cross-read proof COMPLETE — honest infra passes the head-agreement gate.");
    Ok(())
}
