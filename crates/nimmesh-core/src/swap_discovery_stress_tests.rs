//! # swap_discovery_stress_tests — the whole discovery stack, together (G44 + G47 + G51, cfg(test))
//!
//! Many-node proofs that G34–G43 hold up as a system, not just per-goal:
//!  1. Several complementary pairs sharing one ether all DISCOVER (re-advertise G37 → best-rate window
//!     G39) and SETTLE — concurrently, with no cross-talk.
//!  2. A matcher fed a mix of forged (G41), expired (G35), and mis-sized (G40) intents matches NONE of
//!     them, and the observability counters (G42) attribute each drop.
//!  3. (G47) Deterministic recovery under loss via PARTITION/HEAL: a cut pair doesn't discover; healed
//!     within the bounded re-advertise budget it then discovers + settles — and a partition that
//!     OUTLASTS the budget leaves the pair silent (the by-design limit of G37's bounded re-advertise).
//!  4. (G51) An ACTUAL reconnect (peers disconnect, then `on_peer_connected` again) RESETS the
//!     re-advertise budget, so a pair that exhausted it while cut off discovers + settles on reconnect.
//!
//! Deterministic by construction: no probabilistic loss (the per-packet timing of a lossy ether makes
//! "eventually settles within a fixed budget" flaky); discovery is driven by polling every node's
//! maintenance tick, and partition/heal is a hard, RNG-free cut. Each pair is its own link, so a
//! BTC-giver's re-advertised intent only reaches its partner — no accidental cross-pair matches.
//!
//! #84: the "should settle" waits are driven by [`settle`] / [`drive_until`], which fence the node
//! workers and the mock ether to a fixpoint (no wall-clock convergence budget), so these run in CI —
//! they can't flake under `cargo test --all` scheduler oversubscription the way a timed budget did.

use std::sync::Arc;
use std::time::Duration;

use crate::mock_radio::{MeshHarness, MockEther};
use crate::node::MeshNode;
use crate::swap::{LadderParams, SwapPhase};
use crate::swap_discovery_tests::{
    btc_giver_intent_at, intent_for, intent_frame, signed, with_band, FRESH,
};
use crate::swap_intent::Asset;
use crate::swap_node::derive_swap_id;
use crate::swap_session::{NodeIdentity, RatePolicy, DEFAULT_MAX_CONCURRENT_SWAPS};
use crate::test_support::{make_beacon_packet, wait_until, SETTLE};

/// The NIM identity secret for a `mk_identity(tag)` node — its Ed25519 key owns the node's
/// key-derived `nim_address`, so a NIM-giver can authenticate the Propose it originates (S2 / #73).
fn nim_secret(tag: u8) -> [u8; 32] {
    [tag; 32]
}

/// The NIM enclave key for a `mk_identity(tag)` node, wired into a signing participant so its
/// origination flow signs each Propose it floods.
fn mk_enclave_key(tag: u8) -> Arc<dyn crate::nimiq::signer::EnclaveKey> {
    Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
        &nim_secret(tag),
    ))
}

/// A distinct participant identity keyed by `tag` (distinct NIM address + BTC pubkey → distinct swaps).
/// The NIM address is key-derived from [`nim_secret`] so a NIM-giver's Propose self-certifies.
fn mk_identity(tag: u8) -> NodeIdentity {
    let mut btc_pubkey = [tag; 33];
    btc_pubkey[0] = 0x02;
    let nim_pubkey = ed25519_dalek::SigningKey::from_bytes(&nim_secret(tag))
        .verifying_key()
        .to_bytes();
    NodeIdentity {
        nim_address: *crate::nimiq::address::Address::from_public_key(&nim_pubkey).as_bytes(),
        btc_address: vec![tag; 8],
        btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    }
}

// #84: three concurrent pairs settle with no cross-talk. `poll_sync` and mesh delivery ride
// background worker threads (node job queue -> ether worker), which is why a fixed-tick or wall-clock
// convergence budget flaked under `cargo test --all` on CI's 2-core runners. [`drive_until`]/[`settle`]
// fence those threads to a fixpoint instead, so this is deterministic and runs in CI.
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
        // The NIM-giver signs each Propose it originates (S2 / #73); the BTC-giver only responds.
        let nim = h.add_participant_signing(
            &nim_peer,
            &[nim_tag],
            nim_id,
            LadderParams::default(),
            mk_enclave_key(nim_tag),
        );
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
    // Drive every node's tick then fence the mesh to quiescence each round (deterministic — no clock),
    // breaking the instant every pair is Settled.
    let ether = h.ether();
    assert!(
        drive_until(&ether, &nodes, all_settled),
        "every complementary pair should discover its counterparty and settle"
    );

    // G9 (#80): the discovery counters are readable through the FFI-exported accessor — each NIM-giver
    // that matched its counterparty reports it, so the app can surface live discovery state.
    for nim in &nim_nodes {
        let m = nim.discovery_metrics();
        assert!(
            m.matched >= 1 && m.seen >= 1,
            "a matched NIM-giver should report seen/matched discovery metrics over FFI"
        );
    }

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
    // The NIM-giver originates the Propose, so it is a *signing* participant (authenticates each
    // Propose under its NIM key, S2 / #73); the BTC-giver only responds, so it needs no propose key.
    let nim = h.add_participant_signing(
        "nim",
        &[nim_tag],
        nim_id,
        LadderParams::default(),
        mk_enclave_key(nim_tag),
    );
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

/// Safety cap on [`settle`]'s fence passes. A swap handshake is a handful of messages and each pass
/// clears at least one delivery→process→reply hop, so real convergence needs only a few passes; this
/// bound just turns a hypothetical harness bug into a loud failure instead of a hang.
const MAX_SETTLE_PASSES: usize = 64;

/// Safety cap on [`drive_until`]'s tick rounds. The re-advertise schedule fires at ticks 1/3/6/11/19
/// and the best-rate window is 2 ticks, so ~25 rounds is the most any scenario here needs; this bound
/// is generous headroom (the loop breaks the instant `done` holds).
const MAX_DRIVE_ROUNDS: usize = 60;

/// #84: drain the mesh to global quiescence — no wall-clock. Repeatedly fence every node (flushing
/// its worker queue and pushing its outgoing sends into the ether) then fence the ether (delivering
/// every pending transmit into the destination nodes' queues), until a full pass produces zero new
/// transmissions. Because both halves block on a FIFO barrier reply rather than spinning, the test
/// thread yields the CPU to the workers — so this is immune to the CI scheduler oversubscription that
/// made a timed convergence budget flake. (We can't make the ether synchronous — `SpyRadio` asserts
/// the relay's `send` never runs inside `on_packet_received`; fences drain the async path instead.)
fn settle(ether: &MockEther, nodes: &[Arc<MeshNode>]) {
    for _ in 0..MAX_SETTLE_PASSES {
        for n in nodes {
            n.fence();
        }
        let before = ether.enqueued();
        ether.fence();
        for n in nodes {
            n.fence();
        }
        // A pass that delivered every pending transmit and provoked no new send = quiescent.
        if ether.enqueued() == before {
            return;
        }
    }
    panic!("settle: mesh failed to reach quiescence within {MAX_SETTLE_PASSES} fence passes");
}

/// #84: poll every node's maintenance tick then [`settle`] the mesh, up to [`MAX_DRIVE_ROUNDS`],
/// breaking the instant `done` holds. Discovery is tick-driven (re-advertise + best-rate window), so
/// convergence needs several rounds — but each round settles deterministically, so the outcome is a
/// function of protocol state alone, never of timing. Returns whether `done` held.
fn drive_until<F: Fn() -> bool>(ether: &MockEther, nodes: &[Arc<MeshNode>], done: F) -> bool {
    for _ in 0..MAX_DRIVE_ROUNDS {
        for n in nodes {
            n.poll_sync();
        }
        settle(ether, nodes);
        if done() {
            return true;
        }
    }
    done()
}

#[test]
fn a_partitioned_pair_discovers_after_the_link_heals_within_budget() {
    // G47: PARTITION a complementary pair — the BTC-giver re-advertises (G37) but the cut blocks every
    // flood, so nothing discovers. HEAL within the bounded re-advertise budget and the next flood
    // crosses → the pair discovers + settles. Deterministic recovery, no probabilistic loss. (#84:
    // fenced to quiescence each round rather than raced against a wall-clock budget.)
    let mut h = MeshHarness::new();
    let (nim, btc, swap_id) = complementary_pair(&mut h, 0x60, 0x61);
    let ether = h.ether();
    let nodes = [nim.clone(), btc.clone()];
    ether.partition("nim", "btc");

    // Partitioned: a few rounds burn some (blocked) re-advertises; no flood crosses → no discovery.
    for _ in 0..4 {
        for n in &nodes {
            n.poll_sync();
        }
        settle(&ether, &nodes);
    }
    assert!(
        nim.swap_phase(swap_id).is_none(),
        "a partitioned pair must not discover"
    );

    // Heal while the BTC-giver still has re-advertise budget left → the next flood crosses → settle.
    ether.heal("nim", "btc");
    let settled = || {
        nim.swap_phase(swap_id) == Some(SwapPhase::Settled)
            && btc.swap_phase(swap_id) == Some(SwapPhase::Settled)
    };
    assert!(
        drive_until(&ether, &nodes, settled),
        "the healed pair should discover and settle"
    );

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

#[test]
fn a_reconnected_peer_resets_the_re_advertise_budget_and_the_pair_settles() {
    // G51: lift the G47 limit for an ACTUAL reconnect. The pair connects, then the link DROPS (the
    // peers disconnect); the BTC-giver spends its whole re-advertise budget while cut off, so nothing
    // discovers. On RECONNECT the peer set grows, which resets the budget — so the pair now discovers +
    // settles. (Contrast the G47 budget-exhaustion test: `ether.partition` never drops the peer, so the
    // count is unchanged and it stays silent — delivery-loss-without-disconnect keeps the limit.)
    // #84: fenced to quiescence rather than raced against a wall-clock budget.
    let mut h = MeshHarness::new();
    let (nim, btc, swap_id) = complementary_pair(&mut h, 0x64, 0x65);
    let ether = h.ether();
    let nodes = [nim.clone(), btc.clone()];

    // The link drops.
    nim.on_peer_disconnected("btc".to_string());
    btc.on_peer_disconnected("nim".to_string());

    // The BTC-giver burns its whole re-advertise budget while cut off — with no peers its floods reach
    // no one, so nothing crosses. Drain the ticks to quiescence, then confirm nothing discovered.
    for _ in 0..26 {
        btc.poll_sync();
    }
    settle(&ether, &nodes);
    assert!(
        nim.swap_phase(swap_id).is_none(),
        "a disconnected pair must not discover"
    );

    // The link comes back → the peer set grows → the re-advertise budget resets → discover + settle.
    nim.on_peer_connected("btc".to_string());
    btc.on_peer_connected("nim".to_string());
    let settled = || {
        nim.swap_phase(swap_id) == Some(SwapPhase::Settled)
            && btc.swap_phase(swap_id) == Some(SwapPhase::Settled)
    };
    assert!(
        drive_until(&ether, &nodes, settled),
        "after reconnect the reset re-advertise budget should discover + settle"
    );

    h.shutdown();
}

#[test]
fn a_plain_relay_reports_zeroed_discovery_metrics_over_ffi() {
    // G9 (#80): the FFI-exported `discovery_metrics` reader works on ANY node — a plain relay runs no
    // `SwapSession`, so it does no discovery and every counter reads zero. Proves the exported
    // accessor never panics off the participant path (the app calls it on a fresh node).
    let mut h = MeshHarness::new();
    let relay = h.add_node("relay", &[0x01]);
    assert_eq!(
        relay.discovery_metrics(),
        crate::swap_intent::FfiIntentMetrics::default(),
        "a non-participant node reports all-zero discovery metrics"
    );
    h.shutdown();
}
