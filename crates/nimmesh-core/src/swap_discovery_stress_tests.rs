//! # swap_discovery_stress_tests — the whole discovery stack, together (G44 + G47, cfg(test))
//!
//! Many-node proofs that G34–G43 hold up as a system, not just per-goal:
//!  1. Several complementary pairs sharing one ether all DISCOVER (re-advertise G37 → best-rate window
//!     G39) and SETTLE — concurrently, with no cross-talk.
//!  2. A matcher fed a mix of forged (G41), expired (G35), and mis-sized (G40) intents matches NONE of
//!     them, and the observability counters (G42) attribute each drop.
//!  3. (G47) Deterministic recovery under loss via PARTITION/HEAL: a cut pair doesn't discover; healed
//!     within the bounded re-advertise budget it then discovers + settles — and a partition that
//!     OUTLASTS the budget leaves the pair silent (the by-design limit of G37's bounded re-advertise).
//!
//! Deterministic by construction: no probabilistic loss (the per-packet timing of a lossy ether makes
//! "eventually settles within a fixed budget" flaky); discovery is driven by polling every node's
//! maintenance tick, and partition/heal is a hard, RNG-free cut. Each pair is its own link, so a
//! BTC-giver's re-advertised intent only reaches its partner — no accidental cross-pair matches.

use std::sync::Arc;
use std::time::Duration;

use crate::mock_radio::MeshHarness;
use crate::node::MeshNode;
use crate::swap::{LadderParams, SwapPhase};
use crate::swap_discovery_tests::{
    btc_giver_intent_at, intent_for, intent_frame, signed, with_band, FRESH,
};
use crate::swap_intent::Asset;
use crate::swap_node::derive_swap_id;
use crate::swap_session::{NodeIdentity, RatePolicy, DEFAULT_MAX_CONCURRENT_SWAPS};
use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

/// A distinct participant identity keyed by `tag` (distinct NIM address + BTC pubkey → distinct swaps).
fn mk_identity(tag: u8) -> NodeIdentity {
    let mut btc_pubkey = [tag; 33];
    btc_pubkey[0] = 0x02;
    NodeIdentity {
        nim_address: [tag; 20],
        btc_address: vec![tag; 8],
        btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    }
}

#[test]
fn many_complementary_pairs_all_discover_and_settle() {
    // Three NIM-giver / BTC-giver pairs on one ether. Each BTC-giver carries a SIGNED standing intent
    // it re-advertises (G37); its partner picks it up, runs the best-rate window (G39), initiates, and
    // the swap settles. Driven purely by ticking every node — no hand-fed Propose, no loss.
    const PAIRS: usize = 3;
    let mut h = MeshHarness::new();
    let mut nim_nodes = Vec::new();
    let mut btc_nodes = Vec::new();
    let mut swap_ids = Vec::new();
    let mut nodes = Vec::new();

    for i in 0..PAIRS as u8 {
        let nim_tag = 0x10 + i;
        let btc_tag = 0x20 + i;
        let mut nim_id = mk_identity(nim_tag);
        let mut btc_id = mk_identity(btc_tag);
        let nim_intent = intent_for(&nim_id, Asset::Nim, 200_000, 50_000, FRESH);
        let btc_intent = signed(
            intent_for(&btc_id, Asset::Btc, 180_000, 50_000, FRESH),
            btc_tag,
        );
        swap_ids.push(derive_swap_id(&nim_intent, &btc_intent));
        nim_id.standing_intent = Some(nim_intent);
        btc_id.standing_intent = Some(btc_intent);

        let nim_peer = format!("nim{i}");
        let btc_peer = format!("btc{i}");
        let nim = h.add_participant(&nim_peer, &[nim_tag], nim_id, LadderParams::default());
        let btc = h.add_participant(&btc_peer, &[btc_tag], btc_id, LadderParams::default());
        h.connect(&nim_peer, &btc_peer);
        nodes.push(nim.clone());
        nodes.push(btc.clone());
        nim_nodes.push(nim);
        btc_nodes.push(btc);
    }

    let all_settled = || {
        (0..PAIRS).all(|i| {
            nim_nodes[i].swap_phase(swap_ids[i]) == Some(SwapPhase::Settled)
                && btc_nodes[i].swap_phase(swap_ids[i]) == Some(SwapPhase::Settled)
        })
    };
    // Tick every node until all pairs settle (generous budget; no loss → converges quickly).
    let mut settled = false;
    for _ in 0..80 {
        for n in &nodes {
            n.poll_sync();
        }
        if all_settled() {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        settled,
        "every complementary pair should discover its counterparty and settle"
    );

    h.shutdown();
}

#[test]
fn bad_intents_never_produce_a_swap_in_the_mix() {
    // One matcher node fed a forged, an expired, and a mis-sized intent — none may produce a swap, and
    // the discovery counters must attribute each drop. (The throttle/over-cap case is covered in the
    // metrics tests; here every bad intent is a distinct hard reject.)
    let mut nim_id = mk_identity(0x40);
    let nim_intent = with_band(
        intent_for(&nim_id, Asset::Nim, 200_000, 50_000, FRESH),
        50_000,
        500_000,
    );
    nim_id.standing_intent = Some(nim_intent.clone());

    let mut h = MeshHarness::new();
    let nim = h.add_participant("nim", &[0x40], nim_id, LadderParams::default());

    // Head past 1_000 so the expiry-1_000 intent is genuinely stale.
    nim.on_packet_received_from("gw".to_string(), make_beacon_packet([7; 8], 6_000, 5, 7, 1));
    assert!(
        wait_until(|| nim.cached_head_height() == Some(6_000), SETTLE),
        "head should cache"
    );

    let forged = btc_giver_intent_at(0x70, 180_000, 50_000); // crosses + in band, but UNSIGNED
    let expired = signed(
        {
            let mut i = btc_giver_intent_at(0x71, 180_000, 50_000);
            i.expiry_height = 1_000;
            i
        },
        0x71,
    );
    let whale = signed(btc_giver_intent_at(0x72, 5_000_000, 1_250_000), 0x72); // crosses, out of band

    let forged_id = derive_swap_id(&nim_intent, &forged);
    let expired_id = derive_swap_id(&nim_intent, &expired);
    let whale_id = derive_swap_id(&nim_intent, &whale);

    nim.on_packet_received_from("a".to_string(), intent_frame(&forged, [0xC0; 8], 1));
    nim.on_packet_received_from("a".to_string(), intent_frame(&expired, [0xC1; 8], 2));
    nim.on_packet_received_from("a".to_string(), intent_frame(&whale, [0xC2; 8], 3));
    for _ in 0..6 {
        nim.poll_sync();
    }
    std::thread::sleep(Duration::from_millis(40));

    assert!(
        nim.swap_phase(forged_id).is_none(),
        "a forged intent must not match"
    );
    assert!(
        nim.swap_phase(expired_id).is_none(),
        "an expired intent must not match"
    );
    assert!(
        nim.swap_phase(whale_id).is_none(),
        "a mis-sized intent must not match"
    );

    let m = nim.intent_metrics();
    assert_eq!(m.matched, 0, "no bad intent produced a swap");
    assert!(
        m.dropped_signature >= 1,
        "the forged intent is a signature drop"
    );
    assert!(
        m.dropped_expiry >= 1,
        "the expired intent is an expiry drop"
    );
    assert!(
        m.dropped_rate >= 1,
        "the mis-sized intent is a rate/amount drop"
    );

    h.shutdown();
}

/// Build a complementary NIM-giver / BTC-giver pair on one harness — the BTC-giver SIGNS its standing
/// intent so it can re-advertise authentically — connect their link, and return `(nim, btc, swap_id)`.
fn complementary_pair(
    h: &mut MeshHarness,
    nim_tag: u8,
    btc_tag: u8,
) -> (Arc<MeshNode>, Arc<MeshNode>, [u8; 16]) {
    let mut nim_id = mk_identity(nim_tag);
    let mut btc_id = mk_identity(btc_tag);
    let nim_intent = intent_for(&nim_id, Asset::Nim, 200_000, 50_000, FRESH);
    let btc_intent = signed(
        intent_for(&btc_id, Asset::Btc, 180_000, 50_000, FRESH),
        btc_tag,
    );
    let swap_id = derive_swap_id(&nim_intent, &btc_intent);
    nim_id.standing_intent = Some(nim_intent);
    btc_id.standing_intent = Some(btc_intent);
    let nim = h.add_participant("nim", &[nim_tag], nim_id, LadderParams::default());
    let btc = h.add_participant("btc", &[btc_tag], btc_id, LadderParams::default());
    h.connect("nim", "btc");
    (nim, btc, swap_id)
}

/// Tick both nodes one round (drives re-advertise + the match window + the swap), with a beat for the
/// ether to deliver.
fn tick_round(nim: &Arc<MeshNode>, btc: &Arc<MeshNode>) {
    nim.poll_sync();
    btc.poll_sync();
    std::thread::sleep(Duration::from_millis(5));
}

#[test]
fn a_partitioned_pair_discovers_after_the_link_heals_within_budget() {
    // G47: PARTITION a complementary pair — the BTC-giver re-advertises (G37) but the cut blocks every
    // flood, so nothing discovers. HEAL within the bounded re-advertise budget and the next flood
    // crosses → the pair discovers + settles. Deterministic recovery, no probabilistic loss.
    let mut h = MeshHarness::new();
    let (nim, btc, swap_id) = complementary_pair(&mut h, 0x60, 0x61);
    h.ether().partition("nim", "btc");

    // Partitioned: a few rounds burn some (blocked) re-advertises; no flood crosses → no discovery.
    for _ in 0..4 {
        tick_round(&nim, &btc);
    }
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        nim.swap_phase(swap_id).is_none(),
        "a partitioned pair must not discover"
    );

    // Heal while the BTC-giver still has re-advertise budget left → the next flood crosses → settle.
    h.ether().heal("nim", "btc");
    let settled = || {
        nim.swap_phase(swap_id) == Some(SwapPhase::Settled)
            && btc.swap_phase(swap_id) == Some(SwapPhase::Settled)
    };
    let mut ok = false;
    for _ in 0..60 {
        tick_round(&nim, &btc);
        if settled() {
            ok = true;
            break;
        }
    }
    assert!(ok, "the healed pair should discover and settle");

    h.shutdown();
}

#[test]
fn a_partition_outlasting_the_re_advertise_budget_leaves_the_pair_silent() {
    // G47: the BY-DESIGN limit of G37's BOUNDED re-advertise. If the partition outlives the 5 retries
    // (ticks ~1/3/6/11/20), the BTC-giver spends its whole budget while cut off and goes silent — so
    // healing too late never recovers. (A future goal could reset/resume re-advertise on reconnect.)
    let mut h = MeshHarness::new();
    let (nim, btc, swap_id) = complementary_pair(&mut h, 0x62, 0x63);
    h.ether().partition("nim", "btc");

    // Tick well past the 5th re-advertise (tick 20) while partitioned → the budget is fully spent.
    for _ in 0..26 {
        tick_round(&nim, &btc);
    }
    h.ether().heal("nim", "btc");

    // Budget exhausted → the BTC-giver re-advertises no more → nim never discovers it.
    for _ in 0..30 {
        tick_round(&nim, &btc);
    }
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        nim.swap_phase(swap_id).is_none(),
        "a partition past the re-advertise budget leaves the pair silent"
    );

    h.shutdown();
}
