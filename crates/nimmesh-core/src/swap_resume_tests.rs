//! # swap_resume_tests — discovery survives a restart (G43, cfg(test))
//!
//! Two restart guarantees for the discovery layer on top of G33 crash recovery:
//!  1. A restored node keeps its STANDING intent (it rides `NodeIdentity`), so re-advertising (G37)
//!     resumes after restore.
//!  2. A swap that became a real coordinator via the match window (G39) before the crash is a normal
//!     coordinator, so G33's snapshot/restore carries it — the restored node's tick still refunds it.
//!
//! Buffered, not-yet-matched match-window CANDIDATES are intentionally NOT persisted: they carry no
//! funds and no commitment, and they re-arrive on the mesh via re-advertise (G37), so dropping them on
//! restart is free.

use crate::mock_radio::MeshHarness;
use crate::node::MeshNode;
use crate::relay::RelayPolicy;
use crate::swap::{LadderParams, SwapPhase};
use crate::swap_discovery_tests::{
    count_swap_intent_frames, intent_for, intent_frame, signed, FRESH,
};
use crate::swap_intent::Asset;
use crate::swap_node::derive_swap_id;
use crate::test_support::{make_beacon_packet, participant_fixtures, wait_until, SpyRadio, SETTLE};

#[test]
fn a_restored_node_re_advertises_its_standing_intent() {
    // G43 (1): a node restarts (restored from an empty snapshot — no in-flight swaps) and still holds
    // its standing intent, so it resumes re-advertising. Built via the restore path with a SpyRadio so
    // we can count the re-advertised SwapIntent frames.
    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let radio = SpyRadio::new();
    let node = MeshNode::new_participant_restored(
        vec![1],
        radio.clone(),
        RelayPolicy::deterministic(),
        alice_id,
        LadderParams::default(),
        vec![0u8, 0u8], // a valid empty-session snapshot (u16 count = 0)
    );
    node.on_peer_connected("p".to_string());

    for _ in 0..40 {
        node.poll_sync();
    }
    assert!(
        wait_until(|| count_swap_intent_frames(&radio) >= 1, SETTLE),
        "a restored node with a standing intent should resume re-advertising"
    );

    node.shutdown();
}

#[test]
fn a_discovered_funds_locked_swap_survives_restore_and_refunds() {
    // G43 (2): alice DISCOVERS a counterparty over the mesh, initiates, funds her NIM leg, then
    // "crashes". Restored from the snapshot, the discovered swap is back funds-locked and the restored
    // node's tick still refunds it past T_A — discovery → a normal coordinator → G33 recovery covers it.
    use crate::codec::encode;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    use crate::swap_messages::SwapAcceptance;
    use crate::swap_wire::encode_swap;

    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let bob_intent = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH),
        0x22,
    );
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);
    let mut identity = alice_id;
    identity.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], identity.clone(), LadderParams::default());

    // Discover + initiate (window close), then an Accept from the counterparty funds alice's NIM leg.
    alice.on_packet_received_from("a".to_string(), intent_frame(&bob_intent, [0xB0; 8], 1));
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Proposed),
            SETTLE
        ),
        "alice should initiate the discovered swap"
    );

    let accept = SwapAcceptance {
        swap_id,
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: {
            let mut k = [0x22; 33];
            k[0] = 0x03;
            k
        },
    }
    .to_envelope();
    let mut pkt = Packet::new(
        MessageType::SwapAccept,
        [9u8; 8],
        encode_swap(&accept).unwrap(),
    );
    pkt.recipient_id = Some(BROADCAST_RECIPIENT);
    alice.on_packet_received_from("bob".to_string(), encode(&pkt).unwrap());
    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::SelfFunded),
            SETTLE
        ),
        "alice should fund her NIM leg"
    );

    // Crash + restore from the snapshot.
    let snapshot = alice.swap_snapshot();
    let restored =
        h.add_participant_restored("alice2", &[2], identity, LadderParams::default(), snapshot);
    restored.poll_sync(); // first tick rebuilds the observable phase mirror from the restored session
    assert!(
        wait_until(
            || restored
                .swap_phase(swap_id)
                .map(|p| p.has_funds_locked())
                .unwrap_or(false),
            SETTLE
        ),
        "the discovered swap should survive restore funds-locked"
    );

    // The restored node hears a head beacon past T_A (10_000); its refund/GC tick reclaims the leg.
    restored.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7; 8], 10_001, 5, 7, 1),
    );
    assert!(
        wait_until(|| restored.cached_head_height() == Some(10_001), SETTLE),
        "the restored node should cache the head beacon"
    );
    restored.poll_sync();
    assert!(
        wait_until(|| restored.swap_phase(swap_id).is_none(), SETTLE),
        "the restored discovered swap should refund + reap past T_A"
    );

    h.shutdown();
}
