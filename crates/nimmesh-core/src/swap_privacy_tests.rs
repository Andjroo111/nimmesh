//! # swap_privacy_tests — discovery-layer unlinkability (G45, cfg(test))
//!
//! Proves the ephemeral-key mitigation from `docs/swap/DISCOVERY-PRIVACY.md`: an advertiser that signs
//! each intent under a fresh per-advertisement key (and rotates its BTC fields) floods nothing that
//! links to its main wallet or to its other advertisements — yet each intent still authenticates and
//! still discovers + settles. The only thing two ads share is the trade TERMS, which name nobody.

use crate::mock_radio::MeshHarness;
use crate::swap::{LadderParams, SwapPhase};
use crate::swap_discovery_tests::{btc_giver_intent, intent_for, intent_frame, FRESH};
use crate::swap_intent::{sign_intent_ephemeral, Asset};
use crate::swap_node::derive_swap_id;
use crate::test_support::{participant_fixtures, wait_until, SETTLE};

#[test]
fn two_ephemeral_advertisements_share_no_identity_field() {
    // The same logical node advertises the same trade twice, each under a fresh NIM key + rotated BTC
    // fields. Every identity field differs (no same-advertiser correlation), both still verify, and the
    // only shared data is the trade terms.
    let mut ad1 = btc_giver_intent(0x01);
    ad1.btc_address = b"tb1qad1".to_vec();
    sign_intent_ephemeral(&mut ad1, &[0xE1; 32]);

    let mut ad2 = btc_giver_intent(0x02);
    ad2.btc_address = b"tb1qad2".to_vec();
    sign_intent_ephemeral(&mut ad2, &[0xE2; 32]);

    // No identity field links the two advertisements.
    assert_ne!(ad1.nim_pubkey, ad2.nim_pubkey);
    assert_ne!(ad1.nim_address, ad2.nim_address);
    assert_ne!(ad1.signature, ad2.signature);
    assert_ne!(ad1.btc_pubkey, ad2.btc_pubkey);
    assert_ne!(ad1.btc_address, ad2.btc_address);

    // Each is still authentic (a matcher will act on it).
    assert!(ad1.verify_authentic());
    assert!(ad2.verify_authentic());

    // What legitimately leaks is only the terms — they name nobody.
    assert_eq!(ad1.nim_amount, ad2.nim_amount);
    assert_eq!(ad1.btc_amount, ad2.btc_amount);
    assert_eq!(ad1.gives, ad2.gives);
}

#[test]
fn an_ephemeral_keyed_intent_still_discovers_and_settles() {
    // The mitigation must not break matching: an intent signed under an ephemeral NIM key is matched
    // and the swap drives all the way to Settled, exactly as a main-key intent would.
    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut bob_intent = intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH);
    sign_intent_ephemeral(&mut bob_intent, &[0xEE; 32]); // a throwaway NIM key, not bob's main one
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    alice.on_packet_received_from("a".to_string(), intent_frame(&bob_intent, [0xB0; 8], 1));
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "an ephemeral-keyed intent should still settle for alice"
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "an ephemeral-keyed intent should still settle for bob"
    );

    h.shutdown();
}
