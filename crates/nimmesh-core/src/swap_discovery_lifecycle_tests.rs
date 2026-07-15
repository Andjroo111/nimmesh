//! # swap_discovery_lifecycle_tests — runtime control of a node's standing advert (G9 / #80)
//!
//! The discovery FFI lets the app STOP advertising its standing swap intent at runtime without
//! tearing the node down (`MeshNode::stop_advertising`). These drive it over the real
//! [`crate::node::MeshNode`] loop and assert the withdrawn intent is never re-flooded again.

use crate::swap_discovery_tests::{count_swap_intent_frames, intent_for, FRESH};
use crate::swap_intent::Asset;
use crate::test_support::participant_fixtures;

#[test]
fn stop_advertising_withdraws_the_standing_intent() {
    // G9 (#80): a node can stop advertising its standing intent at RUNTIME without tearing itself
    // down — the clean alternative to shutdown()+rebuild. After `stop_advertising()` the maintenance
    // tick must never re-flood the intent again, even though the re-advertise budget was not spent
    // (so the halt is provably the withdrawal, not the natural G37 re-advertise cap).
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;
    use crate::swap::LadderParams;
    use crate::swap_node::DEFAULT_MAX_INTENT_READVERTS;
    use crate::test_support::{wait_until, SpyRadio, SETTLE};
    use std::time::Duration;

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(intent);

    let radio = SpyRadio::new();
    let node = MeshNode::new_participant(
        vec![1],
        radio.clone(),
        RelayPolicy::deterministic(),
        alice_id,
        LadderParams::default(),
    );
    node.on_peer_connected("p".to_string());

    // It advertises at least once — proof the advert is live before we withdraw it.
    for _ in 0..6 {
        node.poll_sync();
    }
    assert!(
        wait_until(|| count_swap_intent_frames(&radio) >= 1, SETTLE),
        "the node should advertise its standing intent before it is withdrawn"
    );

    // Withdraw the advert at runtime — the node stays up. The `StopAdvertising` job is enqueued
    // before the ticks below, so the worker clears the intent before they run.
    node.stop_advertising();
    for _ in 0..8 {
        node.poll_sync();
    }
    std::thread::sleep(Duration::from_millis(40));
    let settled = count_swap_intent_frames(&radio);
    assert!(
        settled < DEFAULT_MAX_INTENT_READVERTS as usize,
        "the test must withdraw before the re-advertise budget is spent (saw {settled} frames)"
    );

    // Many more ticks: a withdrawn intent is never re-advertised again.
    for _ in 0..25 {
        node.poll_sync();
    }
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(
        count_swap_intent_frames(&radio),
        settled,
        "a withdrawn standing intent must never be re-advertised"
    );

    node.shutdown();
}
