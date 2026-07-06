//! # mainnet_e2e_tests — the Andjroo-gated MAINNET mesh payment path, end to end
//!
//! Headless proof of the real-funds delivery role (authorized by Andjroo 2026-07-06):
//! a node pinned to MAINNET anchors to the mainnet gateway's head beacon, signs a
//! mainnet transfer, floods it, and the mainnet gateway (over a [`MockRpc`] — **no real
//! network in `cargo test`, ever**) validates `networkId = 24` and settles it. Plus the
//! mirror-image refusals: a testnet-signed tx is refused by the mainnet gateway exactly
//! like a mainnet tx is refused by the testnet one.
//!
//! Live mainnet broadcasting happens ONLY on Andjroo's Mac node with the explicit
//! `--mainnet` launch flag, carrying a tx that OWNER signed on his own device.

use std::sync::Arc;

use crate::gateway::{MeshGateway, RpcGateway};
use crate::mock_radio::MeshHarness;
use crate::nimiq::signer::InMemoryEnclaveKey;
use crate::nimiq::{AppSigner, TransferIntent};
use crate::rpc::MockRpc;
use crate::settlement::PaymentStatus;
use crate::test_support::{wait_until, SETTLE};
use crate::NetworkId;

fn intent_on(network: NetworkId, value: u64, vsh: u32) -> TransferIntent {
    TransferIntent {
        recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
        value,
        validity_start_height: vsh,
        network,
    }
}

#[test]
fn a_mainnet_signed_transfer_anchors_to_the_mainnet_beacon_and_settles() {
    // Mainnet gateway (MockRpc at a mainnet-ish head) + a mainnet phone node.
    let mut h = MeshHarness::new();
    let rpc = Arc::new(MockRpc::new(9_000_000));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new_mainnet(rpc.clone()));
    let gateway = h.add_gateway_on("gw", &[3], gw, NetworkId::Mainnet);
    let origin = h.add_node_on("origin", &[1], NetworkId::Mainnet);
    h.connect("gw", "origin");

    // The mainnet gateway beacons its head (networkId 24); the mainnet node caches it —
    // this is the offline phone's validity anchor.
    gateway.poll_beacon();
    assert!(
        wait_until(|| origin.cached_head_height() == Some(9_000_000), SETTLE),
        "mainnet node never cached the mainnet gateway's head beacon"
    );

    // The anchored intent is coherently MAINNET: mesh-heard head + mainnet network.
    let intent = origin
        .anchored_intent(
            "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
            250_000,
        )
        .expect("a mainnet head was cached");
    assert_eq!(intent.validity_start_height, 9_000_000);
    assert_eq!(intent.network, NetworkId::Mainnet);

    // Sign it (in-memory enclave here; the Keychain on the phone) and hand it to the mesh.
    let signer = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&[9u8; 32])));
    let signed = signer
        .sign_transfer(intent)
        .expect("signs a mainnet transfer");
    let raw_hex = signed.raw_hex.clone();
    let tx_id = origin.submit_signed_transfer(signed);

    // The mainnet gateway accepts networkId 24, broadcasts, and the receipt settles the
    // origin — the exact bytes the origin signed are what went to the (mock) chain.
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    assert_eq!(rpc.broadcasts(), vec![raw_hex]);

    h.shutdown();
}

#[test]
fn a_testnet_signed_transfer_is_refused_by_the_mainnet_gateway() {
    // The mirror of the testnet gateway's mainnet refusal: a TESTNET node's tx (envelope
    // networkId 5) reaches a MAINNET gateway → Failed receipt, nothing broadcast.
    let mut h = MeshHarness::new();
    let rpc = Arc::new(MockRpc::new(9_000_000));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new_mainnet(rpc.clone()));
    let _gateway = h.add_gateway_on("gw", &[3], gw, NetworkId::Mainnet);
    let origin = h.add_node_on("origin", &[1], NetworkId::Testnet);
    h.connect("gw", "origin");

    let signer = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&[8u8; 32])));
    let signed = signer
        .sign_transfer(intent_on(NetworkId::Testnet, 1_000, 42))
        .expect("signs a testnet transfer");
    let tx_id = origin.submit_signed_transfer(signed);

    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Failed);
    assert!(
        rpc.broadcasts().is_empty(),
        "a testnet tx must never be broadcast to mainnet"
    );

    h.shutdown();
}

#[test]
fn a_mainnet_node_ignores_a_testnet_head_beacon() {
    // Network coherence for the anchor: a mainnet phone must never anchor a real tx to a
    // TESTNET head. A testnet gateway beacons; the mainnet node keeps refusing to anchor.
    let mut h = MeshHarness::new();
    let rpc = Arc::new(MockRpc::new(4_000));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new(rpc)); // testnet gateway
    let gateway = h.add_gateway_on("gw", &[3], gw, NetworkId::Testnet);
    let mainnet_node = h.add_node_on("phone", &[1], NetworkId::Mainnet);
    h.connect("gw", "phone");

    gateway.poll_beacon();
    assert!(
        wait_until(|| gateway.cached_head_height() == Some(4_000), SETTLE),
        "the testnet gateway caches its own beacon"
    );
    // The mainnet node heard the same flood but rejected the wrong-network beacon.
    assert_eq!(mainnet_node.cached_head_height(), None);
    assert!(mainnet_node
        .anchored_intent("NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".into(), 1)
        .is_none());

    h.shutdown();
}
