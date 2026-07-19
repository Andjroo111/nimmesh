//! Zero-head-gate regression tests (the 2026-07-19 G10c soak stall — see `swap_head_gate`).
//!
//! Both drive the REAL `MeshNode` worker loop over the harness with a **live-flagged** signer
//! (a `MockSigner` twin whose `is_live()` answers `true`, riding a live-safe session: a
//! chain-backed `NimHtlcVerifier` over `MockRpc` + a non-sim secret source), so the gate sees
//! exactly what the production FFI doors build — with zero network and zero funds.

use std::sync::Arc;

use crate::mock_radio::MeshHarness;
use crate::nim_verifier::{NimFundingStore, NimHtlcVerifier};
use crate::rpc::MockRpc;
use crate::swap::LadderParams;
use crate::swap_coordinator::SwapContext;
use crate::swap_discovery_tests::{intent_for, intent_frame, signed};
use crate::swap_intent::Asset;
use crate::swap_node::derive_swap_id;
use crate::swap_session::SwapSession;
use crate::swap_signer::{MockSigner, SwapSigner};
use crate::swap_wire::{SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};
use crate::test_support::{
    alice_propose_key, make_beacon_packet, participant_fixtures, wait_until, SETTLE,
};

/// A real-scale testnet head (the 07-19 run's was ~5.06M) — the beacon that annihilated the
/// head-0-minted swap before this gate existed.
const REAL_HEAD: u32 = 5_060_000;
/// Intents must outlive `REAL_HEAD` (the sim `FRESH` = 1M would expire at the real head).
const FRESH_PAST_REAL_HEAD: u64 = 6_000_000;

/// `MockSigner` behaviour, LIVE flag — what the gate keys on. The C1 build gate then requires
/// the session it rides to be live-safe, exactly like the production doors.
struct LiveFlaggedSigner;

impl SwapSigner for LiveFlaggedSigner {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        MockSigner.build_funding(ctx, leg)
    }
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        MockSigner.build_claim(ctx, secret)
    }
    fn note_peer(&self, _: [u8; SWAP_ID_LEN], _: [u8; NIM_ADDRESS_LEN], _: &[u8]) {}
    fn is_live(&self) -> bool {
        true
    }
}

/// A live-safe session (passes `live_safety`): chain-backed NIM verifier over a `MockRpc`,
/// non-sim secret source, default (non-zero) testnet confirmation depths.
fn live_safe_session(identity: crate::swap_session::NodeIdentity) -> SwapSession {
    SwapSession::new(identity, LadderParams::default())
        .with_secret_source(crate::swap_secret::secret_source(
            &crate::swap_secret::test_seed(0x5A),
        ))
        .with_funding_verifier(Box::new(NimHtlcVerifier::new(
            Arc::new(MockRpc::new(0)),
            Arc::new(NimFundingStore::new()),
        )))
}

#[test]
fn a_live_headless_initiator_freezes_its_match_window_until_a_head_is_heard() {
    // The 07-19 stall, initiator half: a LIVE node that has heard no beacon must NOT initiate —
    // head-0 terms (absolute 10_000/5_000) around real money are expired by the first real
    // beacon, the coordinator is reaped, and the #189 tombstone bricks the pair forever. The
    // window must FREEZE instead, then close on the first post-beacon tick with head-anchored
    // terms that SURVIVE further ticks at the real head.
    let (_ids, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH_PAST_REAL_HEAD);
    let bob_intent = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH_PAST_REAL_HEAD),
        0x22,
    ); // 3.6 < 4.0 — crosses
    let swap_id = derive_swap_id(&alice_intent, &bob_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);
    let session = live_safe_session(alice_id).with_propose_signer(alice_propose_key());

    let mut h = MeshHarness::new();
    let alice = h.add_session_participant("alice", &[1], session, Box::new(LiveFlaggedSigner));
    let bob = h.add_participant("bob", &[2], bob_id, LadderParams::default());
    h.connect("alice", "bob");

    // Bob's crossing intent arrives while alice is HEADLESS: buffered, but the window is frozen.
    alice.on_packet_received_from("peer".to_string(), intent_frame(&bob_intent, [9u8; 8], 1));
    for _ in 0..6 {
        alice.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        alice.swap_phase(swap_id).is_none(),
        "a LIVE node with no heard head must not initiate (head-0 terms are the soak trap)"
    );

    // The first real head beacon lands (alice relays it on to bob, so both judge the same head).
    alice.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7u8; 8], REAL_HEAD, 5, 7, 1),
    );
    for _ in 0..4 {
        alice.poll_sync();
    }
    assert!(
        wait_until(|| alice.swap_phase(swap_id).is_some(), SETTLE),
        "the frozen window must close and initiate once a head is heard"
    );
    assert!(
        wait_until(|| bob.swap_phase(swap_id).is_some(), SETTLE),
        "bob must judge the head-anchored terms fundable and accept"
    );

    // The regression's kill shot: further ticks AT the real head must not annihilate the swap
    // (head-0 terms would be stale/refund-reaped right here).
    for _ in 0..4 {
        alice.poll_sync();
        bob.poll_sync();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        alice.swap_phase(swap_id).is_some(),
        "the head-anchored swap must survive ticks at the real head"
    );
    assert!(bob.swap_phase(swap_id).is_some());

    h.shutdown();
}

#[test]
fn a_live_headless_responder_defers_a_propose_until_a_head_is_heard() {
    // The 07-19 stall, responder half: a LIVE responder that has heard no beacon cannot judge a
    // Propose's absolute timelocks (its ladder gate would compare them to head 0) — it must
    // DEFER, and the initiator's slow-tick retransmits must then land the swap once a head
    // arrives. The initiator here is a plain sim node (exempt from the gate) that already knows
    // the real head, so its terms are head-anchored and genuinely fundable.
    let (_ids, alice_id, bob_id, _ctx) = participant_fixtures();
    let alice_intent = intent_for(&alice_id, Asset::Nim, 200_000, 50_000, FRESH_PAST_REAL_HEAD);
    let counter_intent = signed(
        intent_for(&bob_id, Asset::Btc, 180_000, 50_000, FRESH_PAST_REAL_HEAD),
        0x22,
    );
    let swap_id = derive_swap_id(&alice_intent, &counter_intent);

    let mut alice_id = alice_id;
    alice_id.standing_intent = Some(alice_intent);

    let mut h = MeshHarness::new();
    let alice = h.add_participant_signing(
        "alice",
        &[1],
        alice_id,
        LadderParams::default(),
        alice_propose_key(),
    );
    // Alice learns the real head BEFORE the link to bob exists, so bob stays headless.
    alice.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([7u8; 8], REAL_HEAD, 5, 7, 1),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));

    let bob = h.add_session_participant(
        "bob",
        &[2],
        live_safe_session(bob_id),
        Box::new(LiveFlaggedSigner),
    );
    h.connect("alice", "bob");

    // Discovery on alice → a head-anchored Propose floods to the HEADLESS live responder.
    alice.on_packet_received_from(
        "peer".to_string(),
        intent_frame(&counter_intent, [9u8; 8], 2),
    );
    for _ in 0..6 {
        alice.poll_sync();
    }
    assert!(
        wait_until(|| alice.swap_phase(swap_id).is_some(), SETTLE),
        "alice (head-anchored, non-live) must initiate"
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        bob.swap_phase(swap_id).is_none(),
        "a LIVE responder with no heard head must defer the Propose, not judge it against head 0"
    );

    // Bob hears a head → alice's slow-tick retransmit of the same Propose must now be accepted.
    bob.on_packet_received_from(
        "gw".to_string(),
        make_beacon_packet([8u8; 8], REAL_HEAD, 5, 7, 3),
    );
    assert!(
        wait_until(
            || {
                alice.poll_sync(); // re-flood the pending Propose (TTL-32 slow-tick budget)
                bob.poll_sync();
                bob.swap_phase(swap_id).is_some()
            },
            SETTLE
        ),
        "the retransmitted Propose must land once the responder has a head"
    );

    h.shutdown();
}
