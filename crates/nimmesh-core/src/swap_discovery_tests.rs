//! # swap_discovery_tests — counterparty discovery over the mesh (G34, cfg(test))
//!
//! Two strangers find each other: a node floods a [`crate::swap_intent::SwapIntent`], and a node
//! holding the complementary standing intent (mirror trade, crossing rate) turns that discovery into
//! a real swap. These drive it over the actual [`crate::node::MeshNode`] loop — a compatible intent
//! produces a `Propose` that settles, an incompatible one produces nothing.

use crate::swap_intent::{Asset, SwapIntent};
use crate::swap_session::NodeIdentity;
use crate::test_support::participant_fixtures;

/// Build a `SwapIntent` mirroring an identity's addresses.
fn intent_for(id: &NodeIdentity, gives: Asset, nim: u64, btc: u64) -> SwapIntent {
    SwapIntent {
        gives,
        nim_amount: nim,
        btc_amount: btc,
        nim_address: id.nim_address,
        btc_pubkey: id.btc_pubkey,
        btc_address: id.btc_address.clone(),
        network_id: 5,
    }
}

/// Inject a flooded `SwapIntent` packet at `node` (as if a peer broadcast it).
fn flood_intent(node: &crate::node::MeshNode, intent: &SwapIntent, ts: u64) {
    use crate::codec::encode;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    let mut pkt = Packet::new(
        MessageType::SwapIntent,
        [9u8; 8],
        crate::swap_intent::encode_intent(intent),
    );
    pkt.recipient_id = Some(BROADCAST_RECIPIENT);
    pkt.timestamp_ms = ts;
    node.on_packet_received_from("peer".to_string(), encode(&pkt).unwrap());
}

#[test]
fn a_complementary_intent_kicks_off_a_swap_that_settles() {
    // G34: alice holds a standing intent (give 200k NIM for 50k BTC, rate 4.0). Bob floods a
    // complementary intent (give 50k BTC, want 180k NIM, asks 3.6) — it crosses, so alice (the
    // NIM-giver) initiates a Propose; bob accepts and the swap drives to Settled. Discovery → swap.
    use crate::mock_radio::MeshHarness;
    use crate::swap::{LadderParams, SwapPhase};
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000);
    let bob_intent = intent_for(&bob_id, Asset::Btc, 180_000, 50_000); // asks 3.6 < 4.0 → crosses
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    // Bob's intent reaches alice; alice matches it and initiates the swap.
    flood_intent(&alice, &bob_intent, 1);

    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "alice never settled the discovered swap"
    );
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "bob never settled the discovered swap"
    );

    h.shutdown();
}

#[test]
fn an_incompatible_rate_intent_is_not_matched() {
    // G34: bob's intent asks 5.0 NIM/BTC (250k/50k), above alice's 4.0 offer — the rates don't
    // cross, so alice initiates nothing. No coordinator, no Propose.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;

    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000);
    let bob_intent = intent_for(&bob_id, Asset::Btc, 250_000, 50_000); // asks 5.0 > 4.0 → no cross
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let _bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    flood_intent(&alice, &bob_intent, 1);
    // The match never fires, so alice creates no coordinator — give the worker a beat, then confirm.
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(swap_id).is_none(),
        "an incompatible-rate intent must not start a swap"
    );

    h.shutdown();
}
