//! # swap_fast_tick_tests — the ~3 s in-flight heartbeat, proven deterministically (ADR-0005:
//! no wall-clock sleeps — the scheduler is caller-clocked, the chain is a mutable fake, and the
//! node-level path is fenced). Local fixtures per the `swap_*_tests` convention (each file
//! carries its own).

use super::*;
use crate::packet::MessageType;
use crate::swap::{LadderParams, SwapPhase, SwapTerms};
use crate::swap_coordinator::{SwapContext, SwapCoordinator};
use crate::swap_funding_verify::{ConfirmationPolicy, FundingObservation, FundingVerifier};
use crate::swap_intent::Asset;
use crate::swap_leg::sha256;
use crate::swap_messages::SwapProposal;
use crate::swap_rate::RatePolicy;
use crate::swap_session::{NodeIdentity, SessionError, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS};
use crate::swap_wire::{
    encode_swap, SwapEnvelope, SwapKind, SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN, SWAP_ID_LEN,
};
use std::sync::{Arc, Mutex};

// --- local fixtures (mirrors swap_reverify_tests) --------------------------------------------------

fn identity(seed: u8) -> NodeIdentity {
    let mut pk = [seed; BTC_PUBKEY_LEN];
    pk[0] = 0x02;
    NodeIdentity {
        nim_address: [seed; NIM_ADDRESS_LEN],
        btc_address: b"tb1qnode".to_vec(),
        btc_pubkey: pk,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    }
}

fn nim_secret(seed: u8) -> [u8; 32] {
    [seed; 32]
}

fn nim_address(seed: u8) -> [u8; NIM_ADDRESS_LEN] {
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&nim_secret(seed))
        .verifying_key()
        .to_bytes();
    *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes()
}

fn signed(propose: &SwapEnvelope, seed: u8) -> SwapEnvelope {
    let p = SwapProposal::from_envelope(propose).unwrap();
    let (pubkey, sig) = p.sign(&nim_secret(seed));
    p.to_signed_envelope(pubkey, sig)
}

fn ctx(swap_id: [u8; SWAP_ID_LEN], seed: u8) -> SwapContext {
    let mut pk = [seed; BTC_PUBKEY_LEN];
    pk[0] = 0x02;
    SwapContext {
        swap_id,
        terms: SwapTerms {
            nim_timeout: 10_000,
            counterparty_timeout: 5_000,
        },
        hashlock: sha256(&[42u8; 32]),
        nim_address: nim_address(seed),
        btc_address: b"tb1qnode".to_vec(),
        btc_pubkey: pk,
        give_amount: 100_000,
        take_amount: 50_000,
        network_id: 5,
        term_anchor: 0,
    }
}

fn nim_funding_proof(swap_id: [u8; SWAP_ID_LEN]) -> Vec<u8> {
    encode_swap(&crate::swap_messages::tx_envelope(
        swap_id,
        SwapLegId::Nim,
        vec![0x11; 248],
        [0xC1; 32],
    ))
    .unwrap()
}

/// A funding verifier whose observation is MUTATED between polls — the deterministic stand-in
/// for a chain whose depth climbs (or whose `finalized` tag passes the escrow).
#[derive(Clone)]
struct MutVerifier(Arc<Mutex<FundingObservation>>);
impl MutVerifier {
    fn new(obs: FundingObservation) -> Self {
        MutVerifier(Arc::new(Mutex::new(obs)))
    }
    fn set(&self, obs: FundingObservation) {
        *self.0.lock().unwrap() = obs;
    }
}
impl FundingVerifier for MutVerifier {
    fn observe(&self, _expect: &crate::swap_funding_verify::HtlcExpectation) -> FundingObservation {
        self.0.lock().unwrap().clone()
    }
}

fn found(confirmations: u32) -> FundingObservation {
    FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations,
    }
}

/// A responder session awaiting a 10-deep NIM leg, pre-driven to Accepted with a shallow (3)
/// FundingProof already seen — the exact "awaiting counterparty funding" shape the fast tick
/// exists for. Returns the session, its mutable chain, and the swap id.
fn awaiting_responder() -> (SwapSession, MutVerifier, [u8; SWAP_ID_LEN]) {
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    let chain = MutVerifier::new(found(3));
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default())
        .with_counterparty_chain(Asset::Usdc)
        .with_confirmation_policy(ConfirmationPolicy::testnet_defaults().with_nim(10))
        .with_funding_verifier(Box::new(chain.clone()));
    bob.on_message(
        SwapKind::Propose,
        &encode_swap(&signed(&propose, 0x11)).unwrap(),
        0,
    )
    .unwrap();
    // The proof arrives while shallow → correctly refused; the swap now awaits the clock.
    assert!(matches!(
        bob.on_message(SwapKind::FundingProof, &nim_funding_proof(swap_id), 0),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );
    (bob, chain, swap_id)
}

// --- the scheduler --------------------------------------------------------------------------------

#[test]
fn the_scheduler_fires_at_most_once_per_fast_tick() {
    let mut s = FastTickScheduler::new();
    assert!(s.due(0)); // first poll fires.
    assert!(!s.due(1));
    assert!(!s.due(FAST_VERIFY_TICK_MS - 1));
    assert!(s.due(FAST_VERIFY_TICK_MS)); // a full tick later — fires again.
    assert!(!s.due(FAST_VERIFY_TICK_MS + 1));
}

// --- the session-level fast re-verify --------------------------------------------------------------

#[test]
fn fast_reverify_is_idle_free_rate_limited_and_advances_on_depth() {
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    let chain = MutVerifier::new(found(3));
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default())
        .with_counterparty_chain(Asset::Usdc)
        .with_confirmation_policy(ConfirmationPolicy::testnet_defaults().with_nim(10))
        .with_funding_verifier(Box::new(chain.clone()));

    // IDLE poll (no swap at all): consults nothing AND does not burn the rate-limit slot.
    assert!(bob.fast_reverify(0).is_empty());

    bob.on_message(
        SwapKind::Propose,
        &encode_swap(&signed(&propose, 0x11)).unwrap(),
        0,
    )
    .unwrap();
    assert!(matches!(
        bob.on_message(SwapKind::FundingProof, &nim_funding_proof(swap_id), 0),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(bob.verify_note(&swap_id).unwrap().attempts, 1); // the message-time attempt

    // t=1 ms: had the idle poll at t=0 consumed the slot this would be rate-limited — instead
    // it FIRES (attempt 2), re-runs the same fail-closed gate, and correctly still refuses.
    assert!(bob.fast_reverify(1).is_empty());
    assert_eq!(bob.verify_note(&swap_id).unwrap().attempts, 2);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );

    // The chain buries the leg to depth. A poll INSIDE the 3 s window is rate-limited (no
    // consult: the attempt counter holds) — a hammering shim cannot hit the RPC harder.
    chain.set(found(10));
    assert!(bob.fast_reverify(2_000).is_empty());
    assert_eq!(bob.verify_note(&swap_id).unwrap().attempts, 2);

    // The next due poll advances it — seconds after depth, not a 15 s beat later.
    assert_eq!(bob.fast_reverify(1 + FAST_VERIFY_TICK_MS), vec![swap_id]);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );
    // Advanced swaps stop being re-polled (same as the slow tick — RPC stays quiet).
    assert!(bob.fast_reverify(1 + 2 * FAST_VERIFY_TICK_MS).is_empty());
}

#[test]
fn fast_polls_never_touch_the_retransmit_budget() {
    // RETRANSMIT_TTL (32) is counted in SLOW ticks. Run far more fast windows than the whole
    // budget with the swap still awaiting (every due poll really consults + refuses): the
    // pending action must still be alive afterwards — the fast cadence drains nothing.
    let (mut bob, _chain, swap_id) = awaiting_responder();
    bob.record_action(swap_id, MessageType::SwapAccept, vec![0xAB]);
    for i in 0..50u64 {
        let _ = bob.fast_reverify(i * FAST_VERIFY_TICK_MS);
    }
    let out = bob.pending_retransmits();
    assert_eq!(
        out.len(),
        1,
        "50 fast windows must not exhaust the TTL-32 budget"
    );
    assert_eq!(out[0].0, MessageType::SwapAccept);
}

// --- the node-level job path ------------------------------------------------------------------------

#[test]
fn a_fast_poll_advances_an_awaiting_swap_through_the_real_node_loop() {
    // The FFI door end to end: a participant node holding an awaiting swap; the chain reaches
    // depth; ONE poll_swap_fast (fenced, ADR-0005 — no wall-clock waits) re-verifies, advances
    // the coordinator, drives the next money-path action, and re-syncs the phase mirror —
    // with NO beacon emitted and no slow-tick work.
    use crate::mock_radio::{MockEther, MockRadio};
    use crate::node::MeshNode;
    use crate::relay::RelayPolicy;

    let (bob_session, chain, swap_id) = awaiting_responder();
    chain.set(found(10)); // buried to policy depth BEFORE the poll — one tick must suffice.

    let ether = MockEther::new();
    let radio = MockRadio::new("solo", ether.clone());
    let node = MeshNode::build(
        vec![2],
        radio.clone(),
        None,
        RelayPolicy::deterministic(),
        false,
        Some(bob_session),
        Some(Box::new(crate::swap_signer::MockSigner)),
        crate::NetworkId::Testnet,
    );
    radio.bind(Arc::downgrade(&node));

    node.poll_swap_fast();
    node.fence();
    // The responder verified the NIM leg AND (drive_phase_action) funded its own counter leg
    // through the MockSigner in the same fast tick — mirror-visible without any slow tick.
    assert_eq!(node.swap_phase(swap_id), Some(SwapPhase::BothFunded));
    assert_eq!(node.beacon_emitted(), 0); // the fast tick never beacons.

    node.shutdown();
    ether.shutdown();
}
