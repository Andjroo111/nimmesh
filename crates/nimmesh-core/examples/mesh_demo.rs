//! # mesh_demo — the offline pay loop, headless, no network (G13)
//!
//! Runs the whole GOAL.md demo loop — `submit → flood → TTL relay → dedup → gateway →
//! receipt → settled` — entirely in-process against a **mock** gateway, so it needs no
//! internet, no keys, and no real funds. It is the no-network companion to
//! `live_testnet_broadcast.rs` (which proves the same path against the real testnet).
//!
//! The origin is **not** directly linked to the gateway, so the payment must traverse the
//! relay over real nimmesh packets — exactly the offline hop the project exists for.
//!
//! ```text
//! cargo run -p nimmesh-core --example mesh_demo
//! ```
//!
//! Non-money-path: the `txWire` here is an opaque stand-in (no real signing), the gateway is a
//! mock (no broadcast), and everything is testnet by construction.

use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nimmesh_core::default_network;
use nimmesh_core::gateway::MockGateway;
use nimmesh_core::mock_radio::MeshHarness;
use nimmesh_core::settlement::PaymentStatus;

fn main() {
    println!("nimiq.nimmesh — headless mesh pay-loop demo (mock gateway, no network)\n");

    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let _relay = h.add_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");
    println!("topology:  origin <-> relay <-> gateway   (origin has NO direct gateway link)\n");

    // An opaque stand-in for a real signed Nimiq tx (what gets signed in airplane mode).
    let signed = b"opaque-signed-tx-from-airplane-mode".to_vec();
    println!(
        "1. origin signs offline and floods a nimiqTx ({} opaque bytes)",
        signed.len()
    );
    let tx_id = origin.submit_local_tx(signed.clone());
    println!("   txId = {}", to_hex(&tx_id));
    println!("2. the relay carries it onward (TTL hop, blind dedup, source-link excluded)");
    println!("3. the gateway 'broadcasts' it (mock) and floods a receipt back\n");

    // Poll until the receipt settles the origin (honours unconfirmed-until-inclusion).
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        let s = origin.payment_status(tx_id.clone());
        if s != PaymentStatus::Pending || Instant::now() >= deadline {
            break s;
        }
        sleep(Duration::from_millis(10));
    };

    match status {
        PaymentStatus::Settled => {
            println!("✅ SETTLED — relayed across the mesh, broadcast by the gateway, receipt closed the loop.")
        }
        PaymentStatus::Failed => println!("❌ FAILED — the gateway rejected it (NACK)."),
        PaymentStatus::Pending => {
            println!("… still PENDING — no gateway was reachable within the window.")
        }
    }
    println!("   gateway submissions: {}", gw.submission_count());
    println!(
        "   submitted bytes match what the origin signed: {}",
        gw.submissions().first() == Some(&signed)
    );

    h.shutdown();
}

/// Lowercase hex for the demo's txId print.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
