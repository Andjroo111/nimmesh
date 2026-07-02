//! # swap_discovery_ratelimit_tests — per-link protection for the discovery layer (G53, cfg(test))
//!
//! G36's `IntentThrottle` caps by the intent's ORIGIN `sender_id`, so a hostile neighbour can spoof
//! many origins to slip past it. But the G12 per-peer inbound limiter (`PeerRateLimiter`) is applied
//! to EVERY inbound frame in `process_inbound` BEFORE decode/dispatch — including `SwapIntent` — so it
//! already bounds a discovery flood by the immediate LINK (`src`) it arrived on, no matter how many
//! origins are spoofed. This proves it: a neighbour spraying intents far past its token bucket is
//! rate-limited, while a DIFFERENT neighbour's intent (its own bucket) still matches.

use crate::mock_radio::MeshHarness;
use crate::swap::LadderParams;
use crate::swap_discovery_tests::{btc_giver_intent, intent_for, intent_frame, signed, FRESH};
use crate::swap_intent::Asset;
use crate::swap_node::derive_swap_id;
use crate::test_support::{participant_fixtures, wait_until, SETTLE};

#[test]
fn a_flooding_neighbour_is_rate_limited_while_a_good_neighbour_still_matches() {
    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // A hostile neighbour on link "evil" sprays intents with many SPOOFED origins (distinct packet
    // sender_id + ts), far beyond its 256-token bucket — so the per-link limiter throttles it. (The
    // payload is an unsigned/forged intent; it wouldn't match anyway, but most frames are dropped by
    // the limiter before they're even decoded.)
    let spam = btc_giver_intent(0x80);
    for i in 0..1000u64 {
        alice.on_packet_received_from(
            "evil".to_string(),
            intent_frame(&spam, i.to_be_bytes(), 1000 + i),
        );
    }
    assert!(
        wait_until(|| alice.rate_limited() >= 1, SETTLE),
        "a neighbour flooding intents past its bucket should be rate-limited by the per-link limiter"
    );

    // A DIFFERENT neighbour on link "good" sends one legit complementary intent → its own full bucket
    // lets it through, and it still discovers a swap. The flood next door didn't starve it.
    let good = signed(btc_giver_intent(0x81), 0x81);
    let good_id = derive_swap_id(&alice_intent, &good);
    alice.on_packet_received_from("good".to_string(), intent_frame(&good, [0xB0; 8], 9_000));
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(|| alice.swap_phase(good_id).is_some(), SETTLE),
        "a different neighbour's intent should still match (per-link, not global, limiting)"
    );

    h.shutdown();
}
