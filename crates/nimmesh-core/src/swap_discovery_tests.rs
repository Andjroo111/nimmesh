//! # swap_discovery_tests — counterparty discovery over the mesh (G34, cfg(test))
//!
//! Two strangers find each other: a node floods a [`crate::swap_intent::SwapIntent`], and a node
//! holding the complementary standing intent (mirror trade, crossing rate) turns that discovery into
//! a real swap. These drive it over the actual [`crate::node::MeshNode`] loop — a compatible intent
//! produces a `Propose` that settles, an incompatible one produces nothing.

use crate::swap_intent::{Asset, SwapIntent};
use crate::swap_session::NodeIdentity;
use crate::swap_wire::{BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};
use crate::test_support::participant_fixtures;

/// A far-future expiry, so a fresh intent stays fresh at the tests' default head of 0.
const FRESH: u64 = 1_000_000;

/// A fresh BTC-giver intent that crosses alice's 4.0 offer (asks 3.6), with `pubkey_tag` varying the
/// claimant pubkey so each one derives a DISTINCT `swap_id` — i.e. an independent match attempt.
fn btc_giver_intent(pubkey_tag: u8) -> SwapIntent {
    let mut btc_pubkey = [0x33; BTC_PUBKEY_LEN];
    btc_pubkey[0] = 0x02;
    btc_pubkey[1] = pubkey_tag;
    SwapIntent {
        gives: Asset::Btc,
        nim_amount: 180_000,
        btc_amount: 50_000,
        expiry_height: FRESH,
        nim_address: [0xC3; NIM_ADDRESS_LEN],
        btc_pubkey,
        btc_address: b"tb1qflood".to_vec(),
        network_id: 5,
    }
}

/// Build a `SwapIntent` mirroring an identity's addresses, valid through `expiry_height`.
fn intent_for(id: &NodeIdentity, gives: Asset, nim: u64, btc: u64, expiry: u64) -> SwapIntent {
    SwapIntent {
        gives,
        nim_amount: nim,
        btc_amount: btc,
        expiry_height: expiry,
        nim_address: id.nim_address,
        btc_pubkey: id.btc_pubkey,
        btc_address: id.btc_address.clone(),
        network_id: 5,
    }
}

/// A flooded `SwapIntent` wire frame from `sender`, stamped `ts` (distinct `ts`/sender → distinct
/// relay_key, so multiple injected intents don't collide on the dedup cache).
fn intent_frame(intent: &SwapIntent, sender: [u8; 8], ts: u64) -> Vec<u8> {
    use crate::codec::encode;
    use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
    let mut pkt = Packet::new(
        MessageType::SwapIntent,
        sender,
        crate::swap_intent::encode_intent(intent),
    );
    pkt.recipient_id = Some(BROADCAST_RECIPIENT);
    pkt.ttl = crate::packet::DEFAULT_TTL;
    pkt.timestamp_ms = ts;
    encode(&pkt).unwrap()
}

/// Inject a flooded `SwapIntent` at `node` (as if a peer broadcast it).
fn flood_intent(node: &crate::node::MeshNode, intent: &SwapIntent, ts: u64) {
    node.on_packet_received_from("peer".to_string(), intent_frame(intent, [9u8; 8], ts));
}

/// How many `SwapIntent` packets a spy radio was asked to send (decode each frame, filter by type).
fn count_swap_intent_frames(radio: &crate::test_support::SpyRadio) -> usize {
    radio
        .frames()
        .iter()
        .filter_map(|f| crate::codec::decode(f).ok())
        .filter(|p| p.msg_type == crate::packet::MessageType::SwapIntent)
        .count()
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
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let bob_intent = intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH); // asks 3.6 < 4.0 → cross
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
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let bob_intent = intent_for(&bob_id, Asset::Btc, 250_000, 50_000, FRESH); // asks 5.0 > 4.0 → no cross
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

#[test]
fn an_expired_intent_does_not_match_even_when_the_rate_crosses() {
    // G35: bob's intent crosses on rate (asks 3.6 < alice's 4.0) but its expiry_height (1_000) is
    // already behind the chain head (6_000). Freshness is the only thing stopping it, so this proves
    // the expiry gate alone kills the match — alice initiates nothing.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let bob_intent = intent_for(&bob_id, Asset::Btc, 180_000, 50_000, 1_000); // crosses, but stale
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let _bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    // Push alice's head to 6_000 — past the intent's expiry_height of 1_000.
    alice.on_packet_received_from("gw".to_string(), make_beacon_packet([7; 8], 6_000, 5, 7, 1));
    assert!(
        wait_until(|| alice.cached_head_height() == Some(6_000), SETTLE),
        "alice never cached the head beacon"
    );

    flood_intent(&alice, &bob_intent, 1);
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(swap_id).is_none(),
        "an expired intent must not start a swap, even at a crossing rate"
    );

    h.shutdown();
}

#[test]
fn a_relay_forwards_a_fresh_intent_but_drops_an_expired_one() {
    // G35: a pure relay (no swap session) carries the discovery layer like any other flood — but only
    // while the ad is live. With the head at 5_000, a fresh intent (expiry 9_000) is relayed onward,
    // while an expired one (expiry 1_000) is dropped, so a stale ad stops propagating across the mesh.
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;
    use crate::test_support::{make_beacon_packet, wait_until, SpyRadio, SETTLE};

    let (_swap_id, alice_id, bob_id, _ctx) = participant_fixtures();
    let radio = SpyRadio::new();
    let node = MeshNode::new_with_policy(vec![5], radio.clone(), RelayPolicy::deterministic());
    node.on_peer_connected("a".to_string());
    node.on_peer_connected("b".to_string());

    // Head at 5_000.
    node.on_packet_received_from("gw".to_string(), make_beacon_packet([7; 8], 5_000, 5, 7, 1));
    assert!(
        wait_until(|| node.cached_head_height() == Some(5_000), SETTLE),
        "the relay never cached the head beacon"
    );
    std::thread::sleep(std::time::Duration::from_millis(30)); // let the beacon's own relay settle
    let before_fresh = radio.send_count();

    // A fresh intent (expiry 9_000 > head) is carried onward.
    let fresh = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, 9_000);
    node.on_packet_received_from("a".to_string(), intent_frame(&fresh, [1; 8], 2));
    assert!(
        wait_until(|| radio.send_count() > before_fresh, SETTLE),
        "a fresh intent should be relayed onward"
    );
    let after_fresh = radio.send_count();

    // An expired intent (expiry 1_000 < head) is dropped — never relayed.
    let expired = intent_for(&bob_id, Asset::Btc, 180_000, 50_000, 1_000);
    node.on_packet_received_from("a".to_string(), intent_frame(&expired, [2; 8], 3));
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        radio.send_count(),
        after_fresh,
        "an expired intent must not be relayed"
    );

    node.shutdown();
}

#[test]
fn the_intent_throttle_caps_each_sender_independently() {
    // G36 (pure): a sender is admitted up to the per-sender cap, then dropped; a different sender has
    // its own independent budget.
    use crate::swap_node::{IntentThrottle, DEFAULT_INTENT_MATCH_CAP_PER_SENDER};

    let mut t = IntentThrottle::new();
    let a = [1u8; 8];
    let b = [2u8; 8];
    for _ in 0..DEFAULT_INTENT_MATCH_CAP_PER_SENDER {
        assert!(t.admit(a), "a sender should be admitted up to its cap");
    }
    assert!(!t.admit(a), "a sender over its cap must be dropped");
    assert!(!t.admit(a), "and stays dropped");
    assert!(t.admit(b), "a different sender keeps its own budget");
}

#[test]
fn one_flooder_cannot_exhaust_the_matcher_but_another_sender_still_matches() {
    // G36: a hostile node floods many distinct rate-crossing intents (distinct pubkey → distinct
    // swap_id, each a fresh match attempt). The per-sender throttle caps how many alice acts on, so
    // the flooder can't fill all her swap slots — and a legit intent from a DIFFERENT sender still
    // matches. (The concurrency cap is 16, far above the per-sender cap, so the throttle is what
    // bounds this, not the slot count.)
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::{derive_swap_id, DEFAULT_INTENT_MATCH_CAP_PER_SENDER};
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let cap = DEFAULT_INTENT_MATCH_CAP_PER_SENDER as usize;
    let flood = cap + 6; // well over one sender's budget, still under the 16-slot concurrency cap

    // One hostile sender floods `flood` distinct matching intents.
    let flooder = [0xF1; 8];
    let swap_ids: Vec<_> = (0..flood)
        .map(|i| {
            let intent = btc_giver_intent(i as u8 + 1);
            let id = derive_swap_id(&alice_intent, &intent);
            alice.on_packet_received_from(
                "link".to_string(),
                intent_frame(&intent, flooder, 100 + i as u64),
            );
            id
        })
        .collect();

    let admitted = || {
        swap_ids
            .iter()
            .filter(|id| alice.swap_phase(**id).is_some())
            .count()
    };
    assert!(
        wait_until(|| admitted() >= cap, SETTLE),
        "alice should admit up to the per-sender cap"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        admitted(),
        cap,
        "a single flooder must not exceed its per-sender match budget"
    );

    // A legit intent from a DIFFERENT sender still gets through (its own budget).
    let other = btc_giver_intent(0xC8);
    let other_id = derive_swap_id(&alice_intent, &other);
    alice.on_packet_received_from("link2".to_string(), intent_frame(&other, [0xB0; 8], 9_000));
    assert!(
        wait_until(|| alice.swap_phase(other_id).is_some(), SETTLE),
        "a different sender must still match through the throttle"
    );

    h.shutdown();
}

#[test]
fn an_unmatched_node_re_advertises_its_intent_a_bounded_number_of_times() {
    // G37: a node holding a standing intent but with no taker yet re-floods it on the maintenance
    // tick — bounded, with backoff — so a counterparty that only later comes into range can find it.
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;
    use crate::swap::LadderParams;
    use crate::swap_node::DEFAULT_MAX_INTENT_READVERTS;
    use crate::test_support::{wait_until, SpyRadio, SETTLE};

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

    let max = DEFAULT_MAX_INTENT_READVERTS as usize;
    // Drive the maintenance tick well past the backoff schedule.
    for _ in 0..40 {
        node.poll_sync();
    }
    assert!(
        wait_until(|| count_swap_intent_frames(&radio) >= max, SETTLE),
        "an unmatched node should re-advertise up to its cap"
    );
    // The budget is spent — more ticks must not produce more re-advertisements.
    for _ in 0..15 {
        node.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        count_swap_intent_frames(&radio),
        max,
        "re-advertise must be bounded"
    );

    node.shutdown();
}

#[test]
fn a_node_in_a_swap_stops_re_advertising() {
    // G37: once a swap forms (a coordinator exists) the node has found its counterparty, so it must
    // stop re-advertising the standing intent.
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;
    use crate::swap::LadderParams;
    use crate::swap_coordinator::SwapCoordinator;
    use crate::test_support::{wait_until, SpyRadio, SETTLE};

    let (swap_id, alice_id, _bob_id, alice_ctx) = participant_fixtures();
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

    // The node has matched into a swap (a coordinator is registered).
    let (coord, propose) =
        SwapCoordinator::new_initiator(alice_ctx, [42u8; 32], LadderParams::default());
    node.start_swap(swap_id, coord, propose);
    assert!(
        wait_until(|| node.swap_phase(swap_id).is_some(), SETTLE),
        "the coordinator should register"
    );

    for _ in 0..40 {
        node.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        count_swap_intent_frames(&radio),
        0,
        "a node already in a swap must not re-advertise its intent"
    );

    node.shutdown();
}

#[test]
fn an_expired_standing_intent_is_not_re_advertised() {
    // G37 × G35: an expired standing intent is never re-advertised — a stale ad must die, not loop.
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;
    use crate::swap::LadderParams;
    use crate::test_support::{make_beacon_packet, wait_until, SpyRadio, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, 1_000); // expires at height 1_000
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

    // Push the head past the intent's expiry.
    node.on_packet_received_from("gw".to_string(), make_beacon_packet([7; 8], 6_000, 5, 7, 1));
    assert!(
        wait_until(|| node.cached_head_height() == Some(6_000), SETTLE),
        "the head should cache"
    );

    for _ in 0..40 {
        node.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        count_swap_intent_frames(&radio),
        0,
        "an expired intent must not be re-advertised"
    );

    node.shutdown();
}
