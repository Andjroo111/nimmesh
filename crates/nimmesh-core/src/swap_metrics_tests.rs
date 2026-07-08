//! # swap_metrics_tests — discovery-layer observability counters (G42, cfg(test))
//!
//! Proves the read-only [`crate::swap_node::IntentMetrics`] counters track what the intent gates do:
//! every intent a participant sees, the one it matches, and each drop reason (expiry G35, rate/amount
//! G40, bad signature G41, throttle G36) plus re-advertisements (G37). Pure observability — the
//! counters never change behaviour. Shares the intent builders with [`crate::swap_discovery_tests`].

use crate::mock_radio::MeshHarness;
use crate::swap::LadderParams;
use crate::swap_discovery_tests::{btc_giver_intent, intent_for, intent_frame, signed, FRESH};
use crate::swap_intent::Asset;
use crate::test_support::{make_beacon_packet, participant_fixtures, wait_until, SETTLE};

#[test]
fn counters_track_seen_matched_and_each_drop_reason() {
    // Four intents reach alice (a NIM-giver matcher): one authentic+crossing (matches), one expired,
    // one signed-but-non-crossing, and one unsigned (forged). The counters must attribute each.
    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // Head at 6_000 so the expiry-1_000 intent is genuinely stale (G35).
    alice.on_packet_received_from("gw".to_string(), make_beacon_packet([7; 8], 6_000, 5, 7, 1));
    assert!(
        wait_until(|| alice.cached_head_height() == Some(6_000), SETTLE),
        "head should cache"
    );

    let authentic = signed(btc_giver_intent(0x60), 0x60); // crosses (3.6), fresh, signed → matches
    let expired = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, 1_000),
        0x61,
    ); // stale
    let non_crossing = signed(
        intent_for(&bob_id, Asset::Btc, 250_000, 50_000, FRESH),
        0x62,
    ); // 5.0
    let forged = btc_giver_intent(0x63); // never signed

    alice.on_packet_received_from("a".to_string(), intent_frame(&authentic, [0xB0; 8], 1));
    alice.on_packet_received_from("a".to_string(), intent_frame(&expired, [0xB1; 8], 2));
    alice.on_packet_received_from("a".to_string(), intent_frame(&non_crossing, [0xB2; 8], 3));
    alice.on_packet_received_from("a".to_string(), intent_frame(&forged, [0xB3; 8], 4));
    for _ in 0..4 {
        alice.poll_sync(); // close the match window so the authentic one initiates
    }

    assert!(
        wait_until(|| alice.intent_metrics().matched == 1, SETTLE),
        "the authentic crossing intent should be matched"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    let m = alice.intent_metrics();
    assert_eq!(m.seen, 4, "every intent observed is counted");
    assert_eq!(
        m.matched, 1,
        "exactly the one authentic crossing intent matched"
    );
    assert_eq!(m.dropped_expiry, 1, "the stale intent is a dropped_expiry");
    assert_eq!(
        m.dropped_rate, 1,
        "the non-crossing intent is a dropped_rate"
    );
    assert_eq!(
        m.dropped_signature, 1,
        "the unsigned intent is a dropped_signature"
    );
    assert_eq!(
        m.dropped_throttle, 0,
        "four distinct senders never hit the throttle"
    );
    assert!(
        m.readvertised >= 1,
        "alice re-advertised her standing intent while unmatched"
    );

    h.shutdown();
}

#[test]
fn the_throttle_drop_counter_tracks_a_flood() {
    // Seven distinct crossing intents from ONE sender: the per-sender throttle (cap 4) admits four and
    // drops three, which the dropped_throttle counter records. One swap still matches (best of four).
    use crate::swap_node::DEFAULT_INTENT_MATCH_CAP_PER_SENDER;

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let cap = DEFAULT_INTENT_MATCH_CAP_PER_SENDER as usize;
    let flood = cap + 3;
    let flooder = [0xF1; 8];
    for i in 0..flood {
        let intent = signed(btc_giver_intent(i as u8 + 1), i as u8 + 1);
        alice.on_packet_received_from(
            "link".to_string(),
            intent_frame(&intent, flooder, 100 + i as u64),
        );
    }
    // Fence-driven drain (ADR-0005): each fence guarantees the prior inbound frames /
    // sync tick fully processed — the wall-clock wait this used ran over budget on CI.
    alice.fence();
    for _ in 0..4 {
        alice.poll_sync();
        alice.fence();
    }

    assert_eq!(
        alice.intent_metrics().dropped_throttle,
        flood - cap,
        "the over-cap intents are counted as throttle drops"
    );
    let m = alice.intent_metrics();
    assert_eq!(m.seen, flood, "every flooded intent is seen");
    assert_eq!(
        m.matched, 1,
        "the flooder still gets at most one swap per window"
    );

    h.shutdown();
}
