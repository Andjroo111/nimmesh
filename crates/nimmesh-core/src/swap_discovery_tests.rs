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
pub(crate) const FRESH: u64 = 1_000_000;

/// A fresh BTC-giver intent that crosses alice's 4.0 offer (asks 3.6), with `pubkey_tag` varying the
/// claimant pubkey so each one derives a DISTINCT `swap_id` — i.e. an independent match attempt.
pub(crate) fn btc_giver_intent(pubkey_tag: u8) -> SwapIntent {
    let mut btc_pubkey = [0x33; BTC_PUBKEY_LEN];
    btc_pubkey[0] = 0x02;
    btc_pubkey[1] = pubkey_tag;
    SwapIntent {
        gives: Asset::Btc,
        counter_asset: Asset::Btc,
        nim_amount: 180_000,
        btc_amount: 50_000,
        expiry_height: FRESH,
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0u8; 32],
        nim_address: [0xC3; NIM_ADDRESS_LEN],
        btc_pubkey,
        btc_address: b"tb1qflood".to_vec(),
        network_id: 5,
        signature: [0u8; 64],
    }
}

/// Authenticate `intent` (G41) under the Ed25519 secret `[seed; 32]` — fills its pubkey, NIM address,
/// and signature so it passes `verify_authentic`. Apply LAST, after any amount/band mutation.
pub(crate) fn signed(mut intent: SwapIntent, seed: u8) -> SwapIntent {
    crate::swap_intent::sign_intent(&mut intent, &[seed; 32]);
    intent
}

/// A BTC-giver intent (distinct `pubkey_tag`) asking a specific rate: `nim_amount` NIM per
/// `btc_amount` BTC. Lower NIM-per-BTC = a better rate for the NIM-giver (more BTC per NIM).
pub(crate) fn btc_giver_intent_at(pubkey_tag: u8, nim_amount: u64, btc_amount: u64) -> SwapIntent {
    let mut i = btc_giver_intent(pubkey_tag);
    i.nim_amount = nim_amount;
    i.btc_amount = btc_amount;
    i
}

/// Set a node's acceptable NIM trade-size band (G40).
pub(crate) fn with_band(mut i: SwapIntent, min_nim: u64, max_nim: u64) -> SwapIntent {
    i.min_nim = min_nim;
    i.max_nim = max_nim;
    i
}

/// Build a `SwapIntent` mirroring an identity's addresses, valid through `expiry_height`, with a
/// wide-open trade-size band by default (G40 tests narrow it via [`with_band`]).
pub(crate) fn intent_for(
    id: &NodeIdentity,
    gives: Asset,
    nim: u64,
    btc: u64,
    expiry: u64,
) -> SwapIntent {
    SwapIntent {
        gives,
        // Historic NIM⇄BTC discovery tests: a NIM-giver wants BTC; a counter-asset giver names its own.
        counter_asset: if gives == Asset::Nim {
            Asset::Btc
        } else {
            gives
        },
        nim_amount: nim,
        btc_amount: btc,
        expiry_height: expiry,
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0u8; 32],
        nim_address: id.nim_address,
        btc_pubkey: id.btc_pubkey,
        btc_address: id.btc_address.clone(),
        network_id: 5,
        signature: [0u8; 64],
    }
}

/// A flooded `SwapIntent` wire frame from `sender`, stamped `ts` (distinct `ts`/sender → distinct
/// relay_key, so multiple injected intents don't collide on the dedup cache).
pub(crate) fn intent_frame(intent: &SwapIntent, sender: [u8; 8], ts: u64) -> Vec<u8> {
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
pub(crate) fn count_swap_intent_frames(radio: &crate::test_support::SpyRadio) -> usize {
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
    let bob_intent = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH),
        0x22,
    ); // 3.6 < 4.0
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    // Bob's intent reaches alice; she buffers it as a match candidate (G39), then the window close on
    // the maintenance tick initiates the Propose; bob accepts and the swap drives to Settled.
    flood_intent(&alice, &bob_intent, 1);
    for _ in 0..4 {
        alice.poll_sync();
    }

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
    let bob_intent = signed(
        intent_for(&bob_id, Asset::Btc, 250_000, 50_000, FRESH),
        0x22,
    ); // 5.0 > 4.0
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
    let bob_intent = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, 1_000),
        0x22,
    ); // crosses, stale
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
fn a_flooder_yields_one_swap_per_window_and_a_later_sender_still_matches() {
    // G36 + G39: a hostile node sprays many distinct rate-crossing intents in one window. Under
    // best-rate selection the window initiates against just ONE of them (the best), so a flooder can
    // never spin up a coordinator per intent. A legit intent from a DIFFERENT sender, in a later
    // window, still matches. (The per-sender throttle additionally bounds how many of the flooder's
    // candidates even enter the buffer; the pure throttle test covers that in isolation.)
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    // One hostile sender floods 10 distinct matching intents (all in one window).
    let flooder = [0xF1; 8];
    let swap_ids: Vec<_> = (0..10u64)
        .map(|i| {
            let intent = signed(btc_giver_intent(i as u8 + 1), i as u8 + 1);
            let id = derive_swap_id(&alice_intent, &intent);
            alice.on_packet_received_from(
                "link".to_string(),
                intent_frame(&intent, flooder, 100 + i),
            );
            id
        })
        .collect();
    for _ in 0..4 {
        alice.poll_sync(); // close the window
    }

    let initiated = || {
        swap_ids
            .iter()
            .filter(|id| alice.swap_phase(**id).is_some())
            .count()
    };
    assert!(
        wait_until(|| initiated() >= 1, SETTLE),
        "the best flooded candidate should initiate"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        initiated(),
        1,
        "a flooder must get at most one swap per window, not one per intent"
    );

    // A DIFFERENT sender's intent in a later window still matches (its own budget, its own window).
    let other = signed(btc_giver_intent(0xC8), 0xC8);
    let other_id = derive_swap_id(&alice_intent, &other);
    alice.on_packet_received_from("link2".to_string(), intent_frame(&other, [0xB0; 8], 9_000));
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(|| alice.swap_phase(other_id).is_some(), SETTLE),
        "a different sender must still match in a later window"
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

#[test]
fn the_best_rate_candidate_in_a_window_wins_not_the_first() {
    // G39: two crossing candidates arrive in one window. The WORSE rate arrives first, the BETTER
    // second. Best-rate selection must initiate against the better one (most BTC per NIM for alice) —
    // proving the node no longer just takes the first crossing intent it sees.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH); // alice gives up to 4.0
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let bad = signed(btc_giver_intent_at(0x20, 200_000, 50_000), 0x20); // asks 4.0 (just crosses)
    let good = signed(btc_giver_intent_at(0x10, 150_000, 50_000), 0x10); // asks 3.0 → better for alice
    let bad_id = derive_swap_id(&alice_intent, &bad);
    let good_id = derive_swap_id(&alice_intent, &good);

    // Worse first, better second — both in the same window.
    alice.on_packet_received_from("a".to_string(), intent_frame(&bad, [0xB0; 8], 1));
    alice.on_packet_received_from("a".to_string(), intent_frame(&good, [0xB1; 8], 2));
    for _ in 0..4 {
        alice.poll_sync();
    }

    assert!(
        wait_until(|| alice.swap_phase(good_id).is_some(), SETTLE),
        "the better-rate candidate should win the window"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(bad_id).is_none(),
        "the worse-rate candidate must not be initiated"
    );

    h.shutdown();
}

#[test]
fn a_rate_tie_in_a_window_breaks_deterministically() {
    // G39: two candidates with the SAME rate but different pubkeys tie. The tie-break (smaller swap_id)
    // must pick exactly one, deterministically, regardless of arrival order.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let a = signed(btc_giver_intent(0x30), 0x30); // 180k/50k
    let b = signed(btc_giver_intent(0x31), 0x31); // 180k/50k, same rate, different pubkey
    let a_id = derive_swap_id(&alice_intent, &a);
    let b_id = derive_swap_id(&alice_intent, &b);
    let (winner, loser) = if a_id < b_id {
        (a_id, b_id)
    } else {
        (b_id, a_id)
    };

    alice.on_packet_received_from("a".to_string(), intent_frame(&a, [0xB0; 8], 1));
    alice.on_packet_received_from("a".to_string(), intent_frame(&b, [0xB1; 8], 2));
    for _ in 0..4 {
        alice.poll_sync();
    }

    assert!(
        wait_until(|| alice.swap_phase(winner).is_some(), SETTLE),
        "the smaller swap_id should win a rate tie"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(loser).is_none(),
        "only one candidate wins a tie"
    );

    h.shutdown();
}

#[test]
fn a_band_compatible_counterparty_matches() {
    // G40: alice will trade NIM sizes in [50k, 500k] and offers a 200k-NIM swap. A counterparty
    // wanting 180k NIM at a crossing rate is within the band, so the swap initiates.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = with_band(
        intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH),
        50_000,
        500_000,
    );
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let bob = signed(btc_giver_intent_at(0x40, 180_000, 50_000), 0x40); // 180k ∈ [50k,500k], 3.6 crosses
    let swap_id = derive_swap_id(&alice_intent, &bob);
    alice.on_packet_received_from("a".to_string(), intent_frame(&bob, [0xB0; 8], 1));
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(|| alice.swap_phase(swap_id).is_some(), SETTLE),
        "a band-compatible counterparty should match"
    );

    h.shutdown();
}

#[test]
fn a_mis_sized_counterparty_does_not_match_even_at_a_crossing_rate() {
    // G40: alice trades NIM sizes in [50k, 500k]. A whale wanting a 5M-NIM swap and a dust 40-NIM
    // swap both cross on rate (4.0) yet must NOT match — the amount band, not the rate, rejects them.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = with_band(
        intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH),
        50_000,
        500_000,
    );
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let whale = signed(btc_giver_intent_at(0x50, 5_000_000, 1_250_000), 0x50); // 5M > 500k, 4.0 crosses
    let dust = signed(btc_giver_intent_at(0x51, 40, 10), 0x51); // 40 < 50k, 4.0 crosses
    let whale_id = derive_swap_id(&alice_intent, &whale);
    let dust_id = derive_swap_id(&alice_intent, &dust);
    alice.on_packet_received_from("a".to_string(), intent_frame(&whale, [0xB0; 8], 1));
    alice.on_packet_received_from("a".to_string(), intent_frame(&dust, [0xB1; 8], 2));
    for _ in 0..4 {
        alice.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(whale_id).is_none(),
        "a too-large counterparty must not match"
    );
    assert!(
        alice.swap_phase(dust_id).is_none(),
        "a too-small counterparty must not match"
    );

    h.shutdown();
}

#[test]
fn a_forged_intent_is_rejected_at_the_matcher() {
    // G41: a node only acts on an authentically-signed intent. An authentic crossing intent matches;
    // a tampered one (signed then a field changed) and an unsigned one do NOT — a forged advertisement
    // can't make a matcher initiate a swap, even at a crossing rate within the band.
    use crate::mock_radio::MeshHarness;
    use crate::swap::LadderParams;
    use crate::swap_node::derive_swap_id;
    use crate::test_support::{wait_until, SETTLE};

    let (_swap_id, alice_id, _bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH);
    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent.clone());

    let mut h = MeshHarness::new();
    let alice = h.add_participant("alice", &[1], alice_id, LadderParams::default());

    let authentic = signed(btc_giver_intent(0x60), 0x60);
    let mut tampered = signed(btc_giver_intent(0x61), 0x61);
    tampered.nim_amount += 1; // breaks the signature; still crosses on rate, still in band
    let unsigned = btc_giver_intent(0x62); // never signed

    let authentic_id = derive_swap_id(&alice_intent, &authentic);
    let tampered_id = derive_swap_id(&alice_intent, &tampered);
    let unsigned_id = derive_swap_id(&alice_intent, &unsigned);

    alice.on_packet_received_from("a".to_string(), intent_frame(&authentic, [0xB0; 8], 1));
    alice.on_packet_received_from("a".to_string(), intent_frame(&tampered, [0xB1; 8], 2));
    alice.on_packet_received_from("a".to_string(), intent_frame(&unsigned, [0xB2; 8], 3));
    for _ in 0..4 {
        alice.poll_sync();
    }

    assert!(
        wait_until(|| alice.swap_phase(authentic_id).is_some(), SETTLE),
        "an authentically-signed intent should match"
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        alice.swap_phase(tampered_id).is_none(),
        "a tampered (bad-signature) intent must not match"
    );
    assert!(
        alice.swap_phase(unsigned_id).is_none(),
        "an unsigned intent must not match"
    );

    h.shutdown();
}
