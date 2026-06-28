//! # swap_health_tests — discovery health self-check (G55, cfg(test))
//!
//! Tests for the [`crate::swap_health`] derivation (the types + logic were lifted out of here to a
//! real non-test module in G57; these assertions are unchanged). Pure classifier coverage over raw
//! counts, plus an end-to-end check that drives a real node's live counters.

use crate::swap_health::{discovery_health, DiscoveryStatus, DominantDrop};

#[test]
fn health_classifies_each_discovery_state() {
    // discovery_health(seen, matched, rate, expiry, throttle, signature).

    // Idle — nothing seen yet.
    let h = discovery_health(0, 0, 0, 0, 0, 0);
    assert_eq!(h.status, DiscoveryStatus::Idle);
    assert_eq!(h.total_dropped, 0);
    assert_eq!(h.match_rate_pct, 0);
    assert_eq!(h.dominant_drop, DominantDrop::None);

    // Seen but no match, drops mostly wrong-rate/expired → just no counterparty, not abuse.
    let h = discovery_health(5, 0, 3, 2, 0, 0);
    assert_eq!(h.status, DiscoveryStatus::NoCounterpartiesYet);
    assert_eq!(h.dominant_drop, DominantDrop::Rate); // 3 > 2
    assert_eq!(h.total_dropped, 5);
    assert_eq!(h.match_rate_pct, 0);

    // Rejections dominated by forged signatures → possibly under attack.
    let h = discovery_health(10, 0, 1, 0, 2, 5);
    assert_eq!(h.status, DiscoveryStatus::PossiblyUnderAttack);
    assert_eq!(h.dominant_drop, DominantDrop::Signature); // 5 is the max

    // A match makes it healthy, and the match rate is over resolved intents.
    let h = discovery_health(8, 3, 1, 0, 0, 0);
    assert_eq!(h.status, DiscoveryStatus::Healthy);
    assert_eq!(h.total_dropped, 1);
    assert_eq!(h.match_rate_pct, 75); // 3 / (3 + 1)
    assert_eq!(h.dominant_drop, DominantDrop::Rate);
}

#[test]
fn health_match_rate_and_drop_ties_are_deterministic() {
    assert_eq!(discovery_health(2, 1, 3, 0, 0, 0).match_rate_pct, 25); // 1 / (1+3)
    assert_eq!(discovery_health(2, 1, 0, 0, 0, 0).match_rate_pct, 100); // no drops

    // A tie between two non-abuse reasons breaks toward the later candidate (rate over expiry).
    assert_eq!(
        discovery_health(4, 0, 2, 2, 0, 0).dominant_drop,
        DominantDrop::Rate
    );
    // A tie that includes an abuse reason breaks toward flagging abuse.
    let h = discovery_health(4, 0, 2, 0, 0, 2);
    assert_eq!(h.dominant_drop, DominantDrop::Signature);
    assert_eq!(h.status, DiscoveryStatus::PossiblyUnderAttack);
}

#[test]
fn health_reads_a_forged_flood_off_a_real_node_as_an_attack() {
    // End to end: a node fed only forged intents reports PossiblyUnderAttack via the live counters.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_discovery_tests::{btc_giver_intent, intent_for, intent_frame, FRESH};
    use crate::swap_intent::Asset;
    use crate::test_support::{participant_fixtures, wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // Three forged (unsigned) but rate-crossing intents from distinct spoofed origins.
    for i in 0..3u8 {
        let forged = btc_giver_intent(0x90 + i); // never signed → verify_authentic fails
        alice.on_packet_received_from(
            "evil".to_string(),
            intent_frame(&forged, [0xD0 + i; 8], 1 + i as u64),
        );
    }
    assert!(
        wait_until(|| alice.intent_metrics().dropped_signature >= 3, SETTLE),
        "the forged flood should register as signature drops"
    );

    let m = alice.intent_metrics();
    let health = discovery_health(
        m.seen,
        m.matched,
        m.dropped_rate,
        m.dropped_expiry,
        m.dropped_throttle,
        m.dropped_signature,
    );
    assert_eq!(health.status, DiscoveryStatus::PossiblyUnderAttack);
    assert_eq!(health.dominant_drop, DominantDrop::Signature);
    assert_eq!(health.match_rate_pct, 0);

    h.shutdown();
}
