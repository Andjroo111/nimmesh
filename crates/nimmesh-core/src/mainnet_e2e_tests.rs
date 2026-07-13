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

use crate::gateway::{MeshGateway, MockGateway, RpcGateway};
use crate::mock_radio::MeshHarness;
use crate::nimiq::address::Address;
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
fn history_over_mesh_round_trips_through_fragments_on_mainnet() {
    // The Andjroo gap: a Bluetooth-only phone must see its recent TRANSACTIONS, not just
    // its balance. Full path: phone floods a history query -> the mainnet gateway reads
    // 10 rows from its RPC -> the ~716 B answer rides the G6 fragmenter -> the phone
    // reassembles, caches, and serves it over FFI with correct direction flags.
    use crate::rpc::RpcHistoryTx;

    let phone_addr = "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6";
    let other_addr = "NQ87 JY9X JUEE HA17 JNBB HPGM 5ETQ VT1G CVN2";
    let rpc = Arc::new(MockRpc::new(9_100_000));
    rpc.set_history(
        (0..10u8)
            .map(|i| RpcHistoryTx {
                hash: format!("{:064x}", i as u128 + 1),
                from: if i % 2 == 0 { other_addr } else { phone_addr }.to_string(),
                to: if i % 2 == 0 { phone_addr } else { other_addr }.to_string(),
                value: (i as u64 + 1) * 100_000,
                timestamp_ms: 1_700_000_000_000 + i as u64,
                block_number: if i == 0 {
                    None
                } else {
                    Some(9_099_000 + i as u32)
                },
            })
            .collect(),
    );

    let mut h = MeshHarness::new();
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new_mainnet(rpc));
    let _gateway = h.add_gateway_on("gw", &[3], gw, NetworkId::Mainnet);
    let phone = h.add_node_on("phone", &[1], NetworkId::Mainnet);
    h.connect("gw", "phone");

    phone.query_tx_history(phone_addr.to_string());
    assert!(
        wait_until(
            || phone.cached_tx_history(phone_addr.to_string()).len() == 10,
            SETTLE
        ),
        "the phone never heard the fragmented history answer"
    );

    let rows = phone.cached_tx_history(phone_addr.to_string());
    assert_eq!(rows[0].hash, format!("{:064x}", 1u8));
    assert!(rows[0].incoming, "row 0 pays INTO the phone");
    assert!(!rows[0].confirmed, "row 0 is mempool-pending");
    assert!(!rows[1].incoming, "row 1 pays OUT of the phone");
    assert!(rows[1].confirmed);
    assert_eq!(rows[1].counterparty, other_addr);
    assert_eq!(rows.iter().map(|r| r.value_luna).max(), Some(1_000_000));
    assert!(rows.iter().all(|r| r.head_height == 9_100_000));

    h.shutdown();
}

#[test]
fn a_mainnet_node_never_caches_a_testnet_balance_answer() {
    // The 2026-07-12 field bug: the Mac ran as a TESTNET swap responder next to the
    // mainnet phone; it answered the phone's balance query with the address's TESTNET
    // balance (0). The ctx's unpinned BalanceCache adopted the FIRST answer's network,
    // painted "0 NIM" over a funded mainnet wallet, and then rejected every genuine
    // mainnet answer as a network mismatch. The cache is now pinned to the node's own
    // network at build: the foreign answer must never land — even heard first — and a
    // real mainnet answer must still land after it.
    let mut h = MeshHarness::new();
    let tgw = Arc::new(MockGateway::new(NetworkId::Testnet));
    tgw.set_balance(0, 5_300_000); // the address is unfunded on TESTNET
    let gateway = h.add_gateway_on("tgw", &[3], tgw, NetworkId::Testnet);
    let phone = h.add_node_on("phone", &[1], NetworkId::Mainnet);
    h.connect("tgw", "phone");

    let addr = Address::from_bytes([0x11; 20]).to_user_friendly();
    phone.query_balance(addr.clone());
    assert!(
        wait_until(|| gateway.balance_answered() >= 1, SETTLE),
        "the testnet gateway never answered the query"
    );
    // Drain the mesh to quiescence (ADR-0005 fences — no wall-clock) so the answer has
    // provably REACHED the phone before the negative assertion below.
    settle(&h.ether(), &[gateway.clone(), phone.clone()]);
    assert_eq!(
        phone.test_cached_balance(&addr),
        None,
        "a wrong-network balance answer must never be cached"
    );

    // A mainnet gateway joins and answers: the pinned cache accepts the true balance.
    let mgw = Arc::new(MockGateway::new(NetworkId::Mainnet));
    mgw.set_balance(59_500_000, 55_555_555);
    let _mgateway = h.add_gateway_on("mgw", &[4], mgw, NetworkId::Mainnet);
    h.connect("mgw", "phone");
    phone.query_balance(addr.clone());
    assert!(
        wait_until(|| phone.test_cached_balance(&addr).is_some(), SETTLE),
        "the mainnet answer never landed"
    );
    let cached = phone
        .test_cached_balance(&addr)
        .expect("mainnet balance cached");
    assert_eq!(cached.balance, 59_500_000);
    assert_eq!(cached.head_height, 55_555_555);
    assert_eq!(cached.network_id, NetworkId::Mainnet.wire_id());
    h.shutdown();
}

/// ADR-0005 fence drain (the [`crate::swap_discovery_stress_tests`] `settle` shape): fence
/// every node, deliver the ether, fence again — until a full pass moves zero new transmits.
fn settle(ether: &crate::mock_radio::MockEther, nodes: &[Arc<crate::node::MeshNode>]) {
    for _ in 0..64 {
        for n in nodes {
            n.fence();
        }
        let before = ether.enqueued();
        ether.fence();
        for n in nodes {
            n.fence();
        }
        if ether.enqueued() == before {
            return;
        }
    }
    panic!("settle: mesh failed to reach quiescence");
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
