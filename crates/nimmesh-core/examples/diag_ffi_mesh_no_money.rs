//! Diagnostic twin of `live_ffi_mesh_swap` with NO chains and NO money: both nodes are built
//! through the REAL app-facing FFI ctors (`new_live_swap_initiator` / `new_live_swap_responder`)
//! but every RPC url points at an unreachable localhost port, so any funding attempt fails fast
//! and nothing is ever broadcast. Drives the identical poll loop and prints what the example's
//! watcher would see (`active_swaps`) plus the discovery metrics.
//!
//! Built to localize the 2026-07-19 G10c soak stall (`phone None · mac None` for 256 ticks
//! while a real NIM HTLC sat funded): scenario `late` (default) replays it — the swap
//! negotiates while both nodes are HEADLESS, then a real-scale head beacon (~5.06M) lands at
//! tick 8. Pre-fix, that beacon annihilated both coordinators (stale-reap / phase-only refund)
//! and the #189 tombstone kept the pair dead forever. With the `swap_head_gate` fix the live
//! nodes refuse to negotiate until the head is heard, then form a head-anchored swap that
//! survives. Scenario `early` beacons from tick 0 (the always-healthy ordering).
//!
//! Run: `cargo run --example diag_ffi_mesh_no_money --features "polygon-gateway gateway-rpc" [early|late]`

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use nimmesh_core::beacon::HeadBeacon;
use nimmesh_core::gateway::{MeshGateway, Receipt};
use nimmesh_core::mock_radio::{MeshHarness, MockRadio};
use nimmesh_core::nimiq::signer::InMemoryEnclaveKey;
use nimmesh_core::node::MeshNode;
use nimmesh_core::swap_live_ffi::{FfiLiveInitiatorConfig, FfiLiveResponderConfig, LiveLockBook};
use nimmesh_core::transport::MeshError;

/// A gateway with a fixed (real-scale) testnet head — beacons it over the mesh on demand.
struct FixedHeadGateway(u32);

impl MeshGateway for FixedHeadGateway {
    fn submit(&self, _tx_wire: Vec<u8>) -> Result<Receipt, MeshError> {
        Err(MeshError::Gateway("diag gateway never submits".into()))
    }
    fn head_beacon(&self) -> Option<HeadBeacon> {
        Some(HeadBeacon::new(self.0, 5)) // 5 = testnet wire id
    }
}

const GIVE_LUNA: u64 = 500_000;
const TAKE_MICRO_USDC: u64 = 1_000_000;
const HTLC_HEX: &str = "0xb3B3703E07AC897B7E3e864C113a2Fa547D76736";
const USDC_HEX: &str = "0x41E94Eb019C0762f9Bfcf9Fb1E58725BfB0e7582";
// Unreachable on purpose: connection-refused, instantly. No chain, no funds.
const DEAD_NIM_RPC: &str = "http://127.0.0.1:1";
const DEAD_AMOY_RPC: &str = "http://127.0.0.1:1";

fn seed(tag: u8) -> [u8; 32] {
    nimmesh_core::swap_leg::sha256(&[b"nimmesh-diag-seed-v1".as_slice(), &[tag]].concat())
}

fn main() {
    println!("== diag_ffi_mesh_no_money — REAL FFI ctors, dead RPCs, deterministic ether ==\n");

    let mut h = MeshHarness::new();

    let init_radio = MockRadio::new("phone", h.ether());
    let lock_book = LiveLockBook::new();
    let init_cfg = FfiLiveInitiatorConfig {
        nim_luna: GIVE_LUNA,
        usdc_micro: TAKE_MICRO_USDC,
        expiry_height: u64::MAX / 2,
        intent_seed: seed(1).to_vec(),
        evm_claim_address: vec![0xA5; 20],
        evm_gas_secret: seed(2).to_vec(),
        nim_rpc_url: DEAD_NIM_RPC.into(),
        amoy_rpc_url: DEAD_AMOY_RPC.into(),
        htlc_address: HTLC_HEX.into(),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    };
    let initiator = MeshNode::new_live_swap_initiator(
        b"phone".to_vec(),
        init_radio.clone(),
        Arc::new(InMemoryEnclaveKey::from_secret(&seed(3))),
        lock_book.clone(),
        init_cfg,
        None,
    )
    .expect("initiator ctor");
    let initiator = h.adopt("phone", initiator, init_radio);

    let resp_radio = MockRadio::new("mac", h.ether());
    let resp_cfg = FfiLiveResponderConfig {
        usdc_micro: TAKE_MICRO_USDC,
        nim_luna: GIVE_LUNA,
        expiry_height: u64::MAX / 2,
        nim_claim_seed: seed(4).to_vec(),
        evm_funding_secret: seed(5).to_vec(),
        nim_rpc_url: DEAD_NIM_RPC.into(),
        amoy_rpc_url: DEAD_AMOY_RPC.into(),
        htlc_address: HTLC_HEX.into(),
        usdc_address: USDC_HEX.into(),
        delta_safe_blocks: 0,
        min_claim_window_blocks: 0,
    };
    let responder = MeshNode::new_live_swap_responder(
        b"mac".to_vec(),
        resp_radio.clone(),
        resp_cfg,
        Some(DEAD_NIM_RPC.into()), // gateway probe fails fast — same shape as the live run
    )
    .expect("responder ctor");
    let responder = h.adopt("mac", responder, resp_radio);

    // A third node: a REAL-scale head source (testnet head ~5.06M, like the 07-19 run).
    // Scenario A ("early"): beacons from tick 0 — the head is known before discovery.
    // Scenario B ("late", default): first beacon only after the swap negotiated at head 0.
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "late".into());
    let beacon_from = if scenario == "early" { 0 } else { 8 };
    let gw = h.add_gateway("gw", &[9], Arc::new(FixedHeadGateway(5_060_000)));
    h.connect("phone", "gw");
    h.connect("mac", "gw");

    h.connect("phone", "mac");
    println!("mesh up: phone ⇄ mac ⇄ gw (dead RPCs; beacon scenario: {scenario})\n");

    for tick in 0..20u32 {
        initiator.poll_sync();
        responder.poll_sync();
        initiator.poll_beacon();
        responder.poll_beacon();
        if tick >= beacon_from {
            gw.poll_beacon();
        }
        sleep(Duration::from_millis(400));

        let a = initiator
            .active_swaps()
            .first()
            .map(|m| format!("{:?} note={:?}", m.phase, m.verify_note));
        let b = responder
            .active_swaps()
            .first()
            .map(|m| format!("{:?} note={:?}", m.phase, m.verify_note));
        let im = initiator.discovery_metrics();
        let rm = responder.discovery_metrics();
        println!(
            "[t{tick:02}] phone {a:?} · mac {b:?}\n      phone metrics seen={} matched={} rate={} expiry={} throttle={} sig={} readv={}\n      mac   metrics seen={} matched={} rate={} expiry={} throttle={} sig={} readv={}",
            im.seen, im.matched, im.dropped_rate, im.dropped_expiry, im.dropped_throttle, im.dropped_signature, im.readvertised,
            rm.seen, rm.matched, rm.dropped_rate, rm.dropped_expiry, rm.dropped_throttle, rm.dropped_signature, rm.readvertised,
        );
        if !lock_book.locks().is_empty() {
            println!("      lock book: {:?}", lock_book.locks());
        }
    }
    for l in lock_book.locks() {
        println!("lock: {} tx {}", l.contract, l.funding_tx_hash);
    }
    h.shutdown();
}
