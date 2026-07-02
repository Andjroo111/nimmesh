//! # swap_session_tests — the SwapSession router suite, extracted from `swap_session.rs` so the
//! logic module stays well under the 800-line ceiling (matches the repo's `swap_*_tests` convention).

use crate::packet::MessageType;
use crate::swap::{LadderParams, SwapPhase, SwapTerms};
use crate::swap_coordinator::{SwapContext, SwapCoordinator};
use crate::swap_funding_verify::{
    ConfirmationPolicy, FundingObservation, LedgerVerifier, OnChainHtlc, SimVerifier,
};
use crate::swap_leg::sha256;
use crate::swap_messages::SwapProposal;
use crate::swap_session::*;
use crate::swap_wire::{
    decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN,
    SWAP_ID_LEN,
};

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

/// The NIM identity secret owning an initiator fixture's `nim_address` for seed byte `seed` (S2 /
/// #73) — so its Propose can be authenticated. Distinct seeds → distinct keys/addresses.
fn nim_secret(seed: u8) -> [u8; 32] {
    [seed; 32]
}

/// The key-derived NIM address for `seed` (`Blake2b-256(pubkey)[..20]` of its Ed25519 key).
fn nim_address(seed: u8) -> [u8; NIM_ADDRESS_LEN] {
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&nim_secret(seed))
        .verifying_key()
        .to_bytes();
    *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes()
}

/// Sign an initiator's Propose under the NIM key owning its `nim_address` (keyed by `seed`), the way
/// a node's enclave seam does — so the responder's `recv_propose` authenticates its terms.
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
    }
}

fn only(out: Vec<(MessageType, Vec<u8>)>) -> (MessageType, Vec<u8>) {
    assert_eq!(out.len(), 1);
    out.into_iter().next().unwrap()
}

#[test]
fn two_sessions_route_a_full_swap_to_settled() {
    let head = 0;
    let p = LadderParams::default();
    let secret = [42u8; 32];
    let swap_id = [0x7A; SWAP_ID_LEN];

    let mut alice = SwapSession::new(identity(0x11), p);
    let mut bob = SwapSession::new(identity(0x22), p);

    // Alice starts an initiator coordinator + floods the Propose (authenticated under her NIM key).
    let (coord, propose) = SwapCoordinator::new_initiator(ctx(swap_id, 0x11), secret, p);
    alice.add_initiator(swap_id, coord);
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();

    // Bob's session routes the Propose → spins up a responder → returns the Accept.
    let (mt, accept_bytes) = only(
        bob.on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    assert_eq!(mt, MessageType::SwapAccept);
    assert_eq!(bob.len(), 1);

    // Alice's session routes the Accept to her initiator coordinator (no outgoing).
    assert!(alice
        .on_message(SwapKind::Accept, &accept_bytes, head)
        .unwrap()
        .is_empty());

    // Alice funds NIM (node action) → FundingProof → Bob's session routes it.
    let nim_fp = encode_swap(
        &alice
            .coordinator(&swap_id)
            .unwrap()
            .fund(head, vec![0x11; 248], [0xC1; 32])
            .unwrap(),
    )
    .unwrap();
    assert!(bob
        .on_message(SwapKind::FundingProof, &nim_fp, head)
        .unwrap()
        .is_empty());

    // Bob funds BTC → FundingProof → Alice's session routes it (both BothFunded).
    let btc_fp = encode_swap(
        &bob.coordinator(&swap_id)
            .unwrap()
            .fund(head, vec![0x22; 120], [0xC2; 32])
            .unwrap(),
    )
    .unwrap();
    assert!(alice
        .on_message(SwapKind::FundingProof, &btc_fp, head)
        .unwrap()
        .is_empty());
    assert_eq!(
        alice.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::BothFunded
    );
    // Before its timeout a funds-locked swap is kept — the refund isn't due, and it's never
    // "stale" while it holds funds, so the GC tick leaves it alone to finish settling.
    assert!(alice.tick(5_000).is_empty());
    assert_eq!(alice.len(), 1);

    // Alice claims BTC (reveals S) → PreimageReveal → Bob's session routes it; Bob's node
    // extracts S and drives the NIM claim via the coordinator.
    let reveal = encode_swap(
        &alice
            .coordinator(&swap_id)
            .unwrap()
            .claim_and_reveal(head, secret.to_vec(), [0xC3; 32])
            .unwrap(),
    )
    .unwrap();
    assert!(bob
        .on_message(SwapKind::PreimageReveal, &reveal, head)
        .unwrap()
        .is_empty());
    bob.coordinator(&swap_id)
        .unwrap()
        .recv_reveal(
            &decode_swap(SwapKind::PreimageReveal, &reveal).unwrap(),
            secret,
        )
        .unwrap();

    // Both settle.
    alice.coordinator(&swap_id).unwrap().settle().unwrap();
    bob.coordinator(&swap_id).unwrap().settle().unwrap();
    assert_eq!(
        alice.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Settled
    );
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Settled
    );

    // Settled is terminal: reaping frees both slots so the node isn't left holding dead swaps.
    assert_eq!(alice.reap_terminal(), 1);
    assert_eq!(bob.reap_terminal(), 1);
    assert!(alice.is_empty() && bob.is_empty());
}

#[test]
fn two_concurrent_proposes_route_to_separate_coordinators() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    for swap_id in [[0x01; SWAP_ID_LEN], [0x02; SWAP_ID_LEN]] {
        let (_c, propose) =
            SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
        let (mt, _) = only(
            bob.on_message(
                SwapKind::Propose,
                &encode_swap(&signed(&propose, 0x11)).unwrap(),
                0,
            )
            .unwrap(),
        );
        assert_eq!(mt, MessageType::SwapAccept);
    }
    assert_eq!(bob.len(), 2);
    assert!(bob.coordinator(&[0x01; SWAP_ID_LEN]).is_some());
    assert!(bob.coordinator(&[0x02; SWAP_ID_LEN]).is_some());
}

#[test]
fn a_message_for_an_unknown_swap_is_rejected() {
    let mut s = SwapSession::new(identity(0x22), LadderParams::default());
    // An Accept with no matching initiator coordinator → UnknownSwap.
    let accept = crate::swap_messages::SwapAcceptance {
        swap_id: [0xEE; SWAP_ID_LEN],
        nim_address: [0; NIM_ADDRESS_LEN],
        btc_address: b"tb1q".to_vec(),
        btc_pubkey: [0x03; BTC_PUBKEY_LEN],
    }
    .to_envelope();
    assert_eq!(
        s.on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0),
        Err(SessionError::UnknownSwap)
    );
}

// --- G15: hostile-mesh robustness -------------------------------------------------

/// Build a valid `Propose` payload for `swap_id` (the initiator's, identical on every call so a
/// resend is a true replay).
fn propose_bytes(swap_id: [u8; SWAP_ID_LEN]) -> Vec<u8> {
    let (_c, propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    encode_swap(&signed(&propose, 0x11)).unwrap()
}

#[test]
fn a_replayed_propose_does_not_clobber_the_live_coordinator() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0x55; SWAP_ID_LEN];
    let propose = propose_bytes(swap_id);

    // Bob accepts, then observes the initiator's NIM funding → InitiatorFunded.
    only(bob.on_message(SwapKind::Propose, &propose, 0).unwrap());
    let nim_fp = encode_swap(&crate::swap_messages::tx_envelope(
        swap_id,
        SwapLegId::Nim,
        vec![0x11; 248],
        [0xC1; 32],
    ))
    .unwrap();
    bob.on_message(SwapKind::FundingProof, &nim_fp, 0).unwrap();
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );

    // A replayed identical Propose must be dropped (no reply) and must NOT reset the coordinator
    // back to Accepted — the in-flight swap is untouched.
    assert!(bob
        .on_message(SwapKind::Propose, &propose, 0)
        .unwrap()
        .is_empty());
    assert_eq!(bob.len(), 1);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );
}

#[test]
fn a_malformed_payload_creates_no_coordinator() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    // Garbage bytes for every kind → a structured error, never a panic, never a coordinator.
    for kind in [
        SwapKind::Propose,
        SwapKind::Accept,
        SwapKind::FundingProof,
        SwapKind::PreimageReveal,
        SwapKind::Abort,
    ] {
        let _ = bob.on_message(kind, &[0xFF; 7], 0); // Ok or Err — must not panic.
        assert!(bob.is_empty(), "a malformed {kind:?} half-created state");
    }
}

#[test]
fn an_abort_frees_the_slot() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0x66; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    assert_eq!(bob.len(), 1);

    let abort = encode_swap(&crate::swap_messages::abort(swap_id, 0)).unwrap();
    assert!(bob
        .on_message(SwapKind::Abort, &abort, 0)
        .unwrap()
        .is_empty());
    assert!(bob.is_empty());
    // A stray Abort for an unknown swap is a harmless no-op.
    assert!(bob
        .on_message(SwapKind::Abort, &abort, 0)
        .unwrap()
        .is_empty());
}

// --- G16: session expiry / GC tick ------------------------------------------------

#[test]
fn tick_drops_a_stale_unfunded_negotiation_past_its_deadline() {
    // A swap that accepted but never funded is dead once the head passes the point where it could
    // still be safely funded — `T_B − min_claim_window` = 5000 − 1800 = 3200 in the fixtures (#75/G4),
    // NOT the far-later T_A (10_000). Past there `fund()` would refuse, so the slot is reclaimed early.
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0x77; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );

    // At/under the deadline it is kept; once the head passes it the swap is GC'd — and reported as a
    // stale un-funded drop (so the node floods a teardown Abort for it).
    assert!(bob.tick(3_200).is_empty());
    assert_eq!(bob.len(), 1);
    assert_eq!(bob.tick(3_201), vec![swap_id]);
    assert!(bob.is_empty());
}

#[test]
fn tick_keeps_a_fresh_negotiation_before_its_deadline() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0x78; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    assert!(bob.tick(0).is_empty());
    assert_eq!(bob.len(), 1);
}

#[test]
fn tick_refunds_then_reaps_a_funds_locked_swap_past_its_timeout() {
    // G18: a swap stalls with this node's funds locked. The tick's safety exit refunds the leg
    // once the head passes its timeout, then reaps it — "worst case is a refund", not a leak.
    let mut alice = SwapSession::new(identity(0x11), LadderParams::default());
    let swap_id = [0x99; SWAP_ID_LEN];
    let (coord, _propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    alice.add_initiator(swap_id, coord);

    // Drive alice to a funds-locked phase: accept (from a counterparty), then fund the NIM leg.
    let accept = crate::swap_messages::SwapAcceptance {
        swap_id,
        nim_address: [0xB2; NIM_ADDRESS_LEN],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: [0x03; BTC_PUBKEY_LEN],
    }
    .to_envelope();
    alice
        .on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0)
        .unwrap();
    alice
        .coordinator(&swap_id)
        .unwrap()
        .fund(0, vec![0x11; 248], [0xC1; 32])
        .unwrap();
    assert!(alice
        .coordinator(&swap_id)
        .unwrap()
        .phase()
        .has_funds_locked());

    // The counterparty never funds. Before alice's T_A (10_000) the swap is kept; once the head
    // passes it, the tick refunds her leg and reaps it. A refund-reaped swap is NOT reported as
    // a stale abort (no teardown message — the counterparty handles its own refund).
    assert!(alice.tick(5_000).is_empty());
    assert_eq!(alice.len(), 1);
    assert!(alice.tick(10_001).is_empty());
    assert!(alice.is_empty());
}

// --- G19: abort emission + symmetric teardown ------------------------------------

/// Drive a fresh session's coordinator to a funds-locked phase (initiator funds NIM). Returns
/// the session + the swap_id.
fn funds_locked_session() -> (SwapSession, [u8; SWAP_ID_LEN]) {
    let mut alice = SwapSession::new(identity(0x11), LadderParams::default());
    let swap_id = [0xAB; SWAP_ID_LEN];
    let (coord, _propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    alice.add_initiator(swap_id, coord);
    let accept = crate::swap_messages::SwapAcceptance {
        swap_id,
        nim_address: [0xB2; NIM_ADDRESS_LEN],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: [0x03; BTC_PUBKEY_LEN],
    }
    .to_envelope();
    alice
        .on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0)
        .unwrap();
    alice
        .coordinator(&swap_id)
        .unwrap()
        .fund(0, vec![0x11; 248], [0xC1; 32])
        .unwrap();
    (alice, swap_id)
}

#[test]
fn cancel_emits_an_abort_and_frees_an_unfunded_swap() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0xCD; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    let abort = bob
        .cancel(&swap_id)
        .expect("an un-funded swap can be cancelled");
    assert_eq!(abort.swap_id, swap_id);
    assert!(bob.is_empty());
    // Cancelling an unknown swap is a no-op (no abort).
    assert!(bob.cancel(&swap_id).is_none());
}

#[test]
fn cancel_refuses_a_funds_locked_swap() {
    // A funded swap must REFUND, never abort — cancel returns None and keeps the coordinator.
    let (mut alice, swap_id) = funds_locked_session();
    assert!(alice.cancel(&swap_id).is_none());
    assert_eq!(alice.len(), 1);
}

#[test]
fn an_inbound_abort_never_drops_a_funds_locked_coordinator() {
    // A counterparty abort must not strand our funded leg — we keep it (it refunds via timeout).
    let (mut alice, swap_id) = funds_locked_session();
    let abort = encode_swap(&crate::swap_messages::abort(swap_id, 0)).unwrap();
    assert!(alice
        .on_message(SwapKind::Abort, &abort, 0)
        .unwrap()
        .is_empty());
    assert_eq!(alice.len(), 1);
    assert!(alice
        .coordinator(&swap_id)
        .unwrap()
        .phase()
        .has_funds_locked());
}

// --- G20: pending-action retransmit ----------------------------------------------

#[test]
fn a_pending_action_retransmits_until_its_budget_runs_out() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0xE1; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    bob.record_action(swap_id, MessageType::SwapAccept, vec![1, 2, 3]);
    // Re-flooded every tick for its whole budget, then it expires (so a vanished counterparty
    // doesn't draw retransmits forever).
    for _ in 0..RETRANSMIT_TTL {
        assert_eq!(bob.pending_retransmits().len(), 1);
    }
    assert!(bob.pending_retransmits().is_empty());
}

#[test]
fn tearing_down_a_swap_stops_its_retransmits() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0xE2; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    bob.record_action(swap_id, MessageType::SwapAccept, vec![1]);
    // A local cancel drops the pending action (nothing left to retransmit).
    bob.cancel(&swap_id);
    assert!(bob.pending_retransmits().is_empty());
}

#[test]
fn a_stale_gc_drops_the_pending_retransmit() {
    let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
    let swap_id = [0xE3; SWAP_ID_LEN];
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
            .unwrap(),
    );
    bob.record_action(swap_id, MessageType::SwapAccept, vec![1]);
    // GC past the deadline drops the un-funded swap AND its pending action (we flood an Abort
    // for it instead, not a retransmit of the dead negotiation).
    assert_eq!(bob.tick(10_001), vec![swap_id]);
    assert!(bob.pending_retransmits().is_empty());
}

// --- S1 / #72: the mesh-path funding-verification gate ----------------------------------------

/// Build a `FundingProof` payload for the initiator's NIM leg of `swap_id`.
fn nim_funding_proof(swap_id: [u8; SWAP_ID_LEN]) -> Vec<u8> {
    encode_swap(&crate::swap_messages::tx_envelope(
        swap_id,
        SwapLegId::Nim,
        vec![0x11; 248],
        [0xC1; 32],
    ))
    .unwrap()
}

#[test]
fn a_funding_proof_advances_a_responder_only_when_the_verifier_confirms_it() {
    let head = 0;
    let p = LadderParams::default();
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) = SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], p);
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();
    let nim_fp = nim_funding_proof(swap_id);

    // A responder whose verifier finds NO on-chain NIM HTLC. It accepts, then a FundingProof arrives
    // — but with nothing verified on-chain it must REFUSE to advance (returns the error, which the
    // node treats as "no reply") and stay at Accepted, so it never funds its BTC leg on the message.
    let mut bob_reject = SwapSession::new(identity(0x22), p)
        .with_funding_verifier(Box::new(SimVerifier::returning(FundingObservation::Absent)));
    only(
        bob_reject
            .on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    assert_eq!(
        bob_reject.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );
    assert!(matches!(
        bob_reject.on_message(SwapKind::FundingProof, &nim_fp, head),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(
        bob_reject.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted // the anti-theft invariant: never advanced, so never funds BTC
    );

    // The honest path: the default verifier (sim = accept-all) confirms the funding, so the very
    // same FundingProof advances the responder to InitiatorFunded (now it may fund BTC).
    let mut bob_ok = SwapSession::new(identity(0x22), p);
    only(
        bob_ok
            .on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    bob_ok
        .on_message(SwapKind::FundingProof, &nim_fp, head)
        .unwrap();
    assert_eq!(
        bob_ok.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );
}

#[test]
fn the_mesh_gate_applies_the_per_chain_confirmation_policy_depth() {
    // #74 / G3: the session verifies the initiator's NIM leg at the *policy's* NIM depth, not a flat
    // floor. With a policy demanding 3 NIM confirmations, a 2-deep on-chain HTLC is refused; a 3-deep
    // one advances the responder (only then may it fund BTC). Two sessions (no shared mutability)
    // stand in for the same HTLC observed at two depths — the shallow one is what a reorg would
    // re-expose (the reorg refusal itself is unit-covered in `swap_funding_verify`).
    let head = 0;
    let p = LadderParams::default();
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) = SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], p);
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();
    let nim_fp = nim_funding_proof(swap_id);

    // The initiator's on-chain NIM HTLC pays Bob (the responder) under the agreed hashlock.
    // The recipient the responder claims to is its own `identity.nim_address` (the session builds the
    // responder ctx from its identity), which is the raw seed address, not the key-derived one.
    let nim_htlc = |confs| OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock: sha256(&[42u8; 32]),
        recipient: [0x22u8; NIM_ADDRESS_LEN].to_vec(),
        amount: 100_000,
        timeout: 10_000,
        confirmations: confs,
    };
    let policy = ConfirmationPolicy::testnet_defaults().with_nim(3);

    // Only 2 deep vs a policy of 3 → refuse and stay Accepted (never funds BTC).
    let mut shallow = LedgerVerifier::new();
    shallow.fund(nim_htlc(2));
    let mut bob_shallow = SwapSession::new(identity(0x22), p)
        .with_confirmation_policy(policy)
        .with_funding_verifier(Box::new(shallow));
    only(
        bob_shallow
            .on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    assert!(matches!(
        bob_shallow.on_message(SwapKind::FundingProof, &nim_fp, head),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(
        bob_shallow.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );

    // Buried to the policy depth (3) → advances to InitiatorFunded.
    let mut deep = LedgerVerifier::new();
    deep.fund(nim_htlc(3));
    let mut bob_deep = SwapSession::new(identity(0x22), p)
        .with_confirmation_policy(policy)
        .with_funding_verifier(Box::new(deep));
    only(
        bob_deep
            .on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    bob_deep
        .on_message(SwapKind::FundingProof, &nim_fp, head)
        .unwrap();
    assert_eq!(
        bob_deep.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );
}
