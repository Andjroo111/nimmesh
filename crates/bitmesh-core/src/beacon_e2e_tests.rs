//! # beacon_e2e_tests — G9 head-beacon + validity-window guard, end to end
//!
//! Drives the G9 wiring through the live engine + harness (not just the [`crate::beacon`]
//! unit module): a gateway emits a `nimiqHeadBeacon` (`0x32`), a node caches the head and
//! anchors a signed intent to it, an expired `nimiqTx` is GC'd (never relayed/stored), and
//! the head cache stays monotonic. All deterministic — no real clocks/sleeps on the logic
//! path. Shares the wire-frame builders + spy radio with [`crate::e2e_tests`] via
//! [`crate::test_support`].

use std::sync::Arc;
use std::time::Duration;

use crate::gateway::{MeshGateway, RpcGateway};
use crate::mock_radio::MeshHarness;
use crate::nimiq::signer::InMemoryEnclaveKey;
use crate::nimiq::{AppSigner, VALIDITY_WINDOW};
use crate::node::MeshNode;
use crate::radio::BleRadio;
use crate::relay::RelayPolicy;
use crate::rpc::MockRpc;
use crate::test_support::{
    make_beacon_packet, make_tx_packet_valid_until, wait_until, SpyRadio, SETTLE,
};

#[test]
fn gateway_beacon_anchors_a_signed_intent_to_the_cached_head() {
    // (a) A gateway floods a `nimiqHeadBeacon` (0x32) sourced from its RPC head height; a
    // connected node caches it; a subsequent signed intent anchors `validityStartHeight`
    // to exactly that cached head (RISKS.md #1: anchor to the latest known head).
    let mut h = MeshHarness::new();
    let rpc = Arc::new(MockRpc::new(8_321));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new(rpc.clone()));
    let gateway = h.add_gateway("gw", &[3], gw);
    let node = h.add_node("node", &[1]);
    h.connect("gw", "node");

    // The gateway emits its head beacon; the node caches the height it carried.
    gateway.poll_beacon();
    assert!(
        wait_until(|| node.cached_head_height() == Some(8_321), SETTLE),
        "node never cached the gateway's head beacon"
    );
    assert_eq!(gateway.beacon_emitted(), 1);

    // A subsequent intent anchors to the freshest cached head …
    let intent = node
        .anchored_intent(
            "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
            100_000,
        )
        .expect("a head was cached, so an intent anchors");
    assert_eq!(intent.validity_start_height, 8_321);

    // … and the actual signed transfer carries that same validityStartHeight + window.
    let key = Arc::new(InMemoryEnclaveKey::from_secret(&[7u8; 32]));
    let signer = AppSigner::new(key);
    let signed = signer.sign_transfer(intent).expect("signs");
    assert_eq!(signed.validity_start_height, 8_321);
    assert_eq!(signed.valid_until_height, 8_321 + VALIDITY_WINDOW);

    h.shutdown();
}

#[test]
fn deep_offline_node_refuses_to_anchor_without_a_beacon() {
    // RISKS.md #1: a node that has heard no head beacon must not pre-date a tx to a
    // stale/zero head — `anchored_intent` refuses until it has a fresh head.
    let mut h = MeshHarness::new();
    let node = h.add_node("node", &[1]);
    assert_eq!(node.cached_head_height(), None);
    assert!(node.anchored_intent("NQ…".to_string(), 1).is_none());
    h.shutdown();
}

#[test]
fn node_gcs_a_tx_past_its_validity_window() {
    // (b) After hearing a head beacon, a node DROPS (GC's) a `nimiqTx` whose validity
    // window has already closed (`validUntil <= head`) — it neither relays nor stores it.
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new_with_policy(vec![9], radio, RelayPolicy::deterministic());
    node.on_peer_connected("src".to_string());
    node.on_peer_connected("other".to_string());

    // The node hears a head beacon putting the chain head at 10_000.
    node.on_packet_received_from(
        "src".to_string(),
        make_beacon_packet([5; 8], 10_000, 5, 7, 1),
    );
    assert!(wait_until(
        || node.cached_head_height() == Some(10_000),
        Duration::from_secs(1)
    ));
    // The beacon itself is relayed once (to the non-source peer "other"); baseline that.
    assert!(wait_until(|| spy.send_count() >= 1, Duration::from_secs(1)));
    let after_beacon = spy.send_count();

    // Now a nimiqTx whose window already closed (validUntil 9_000 <= head 10_000) arrives.
    node.on_packet_received_from(
        "src".to_string(),
        make_tx_packet_valid_until([1; 8], b"dead-tx", 9_000, 7, 2),
    );
    assert!(
        wait_until(|| node.expired_dropped() >= 1, Duration::from_secs(1)),
        "an expired tx was not GC'd"
    );
    // It is neither relayed nor stored — no send beyond the beacon's relay.
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        spy.send_count(),
        after_beacon,
        "an expired tx must never be relayed"
    );
    node.shutdown();
}

#[test]
fn a_live_tx_inside_the_window_is_still_relayed() {
    // The guard's complement: a tx still inside its window (validUntil > head) is relayed
    // normally — the GC drops only genuinely-dead txs, never live ones.
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new_with_policy(vec![9], radio, RelayPolicy::deterministic());
    node.on_peer_connected("src".to_string());
    node.on_peer_connected("other".to_string());

    node.on_packet_received_from(
        "src".to_string(),
        make_beacon_packet([5; 8], 1_000, 5, 7, 1),
    );
    assert!(wait_until(
        || node.cached_head_height() == Some(1_000),
        Duration::from_secs(1)
    ));
    let after_beacon = spy.send_count();

    // validUntil 5_000 > head 1_000 → alive → relayed.
    node.on_packet_received_from(
        "src".to_string(),
        make_tx_packet_valid_until([1; 8], b"live-tx", 5_000, 7, 2),
    );
    assert!(
        wait_until(|| spy.send_count() > after_beacon, Duration::from_secs(1)),
        "a live tx inside its window was not relayed"
    );
    assert_eq!(node.expired_dropped(), 0, "a live tx must not be GC'd");
    node.shutdown();
}

#[test]
fn newer_beacon_advances_head_older_is_ignored() {
    // (c) Monotonic head cache: a newer beacon advances the cached head; a stale (older)
    // beacon is ignored and never rolls it back.
    let mut h = MeshHarness::new();
    let b = h.add_node("b", &[2]);

    b.on_packet_received_from("a".to_string(), make_beacon_packet([9; 8], 200, 5, 7, 1));
    assert!(wait_until(
        || b.cached_head_height() == Some(200),
        Duration::from_secs(1)
    ));

    // A stale beacon (height 100 < 200) must not roll the head back.
    b.on_packet_received_from("a".to_string(), make_beacon_packet([9; 8], 100, 5, 7, 2));
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        b.cached_head_height(),
        Some(200),
        "an older beacon must not roll the head back"
    );

    // A newer beacon (height 250 > 200) still advances it.
    b.on_packet_received_from("a".to_string(), make_beacon_packet([9; 8], 250, 5, 7, 3));
    assert!(wait_until(
        || b.cached_head_height() == Some(250),
        Duration::from_secs(1)
    ));

    h.shutdown();
}
