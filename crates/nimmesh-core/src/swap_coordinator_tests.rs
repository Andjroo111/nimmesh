//! # swap_coordinator_tests — the SwapCoordinator suite, extracted from `swap_coordinator.rs` so the
//! logic module stays under the 800-line ceiling (matches the repo's `swap_*_tests` convention, #92).
//!
//! Covers the two-coordinator happy path, the S1 funding-verification gate (never fund / reveal on a
//! message alone), and the S2 / #73 authenticated-Propose gate (`recv_propose` rejects an unsigned,
//! tampered, or forged Propose and accepts a valid signed one).

use crate::swap::{LadderParams, SwapError, SwapPhase, SwapTerms};
use crate::swap_coordinator::{CoordError, SwapContext, SwapCoordinator};
use crate::swap_funding_verify::FundingRejected;
use crate::swap_leg::sha256;
use crate::swap_messages::{tx_envelope, SwapProposal};
use crate::swap_wire::{
    decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN,
    SWAP_ID_LEN,
};

/// The NIM identity secret owning a fixture's `nim_address` for the seed byte `nim` — so an
/// initiator's Propose can be authenticated (S2 / #73). Distinct seeds → distinct keys/addresses.
fn nim_secret(nim: u8) -> [u8; 32] {
    [nim; 32]
}

/// The key-derived NIM address for seed `nim` (`Blake2b-256(pubkey)[..20]` of its Ed25519 key).
fn nim_address(nim: u8) -> [u8; NIM_ADDRESS_LEN] {
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&nim_secret(nim))
        .verifying_key()
        .to_bytes();
    *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes()
}

fn ctx(seed: u8, nim: u8) -> SwapContext {
    let mut pk = [seed; BTC_PUBKEY_LEN];
    pk[0] = 0x02;
    SwapContext {
        swap_id: [0x7A; SWAP_ID_LEN],
        terms: SwapTerms {
            nim_timeout: 10_000,
            counterparty_timeout: 5_000,
        },
        hashlock: sha256(&[42u8; 32]),
        nim_address: nim_address(nim),
        btc_address: b"tb1qnode".to_vec(),
        btc_pubkey: pk,
        give_amount: 100_000,
        take_amount: 50_000,
        network_id: 5,
    }
}

/// Sign an initiator's Propose the way the node's enclave seam does — under the NIM key that owns the
/// Propose's `nim_address` (keyed by the fixture's `nim` seed) — so a responder authenticates it.
fn signed(propose: &SwapEnvelope, nim: u8) -> SwapEnvelope {
    let p = SwapProposal::from_envelope(propose).unwrap();
    let (pubkey, sig) = p.sign(&nim_secret(nim));
    p.to_signed_envelope(pubkey, sig)
}

/// Round-trip an envelope through the wire codec (so the test exercises real bytes, not structs).
fn wire(kind: SwapKind, env: &SwapEnvelope) -> SwapEnvelope {
    decode_swap(kind, &encode_swap(env).unwrap()).unwrap()
}

#[test]
fn two_coordinators_drive_a_full_swap_by_exchanging_envelopes() {
    let head = 0;
    let p = LadderParams::default();
    let secret = [42u8; 32];

    let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), secret, p);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);

    // Propose → Accept (pubkeys exchanged over the wire; the Propose is authenticated).
    let accept = bob
        .recv_propose(&wire(SwapKind::Propose, &signed(&propose, 0xA1)), head)
        .unwrap();
    alice
        .recv_accept(&wire(SwapKind::Accept, &accept), head)
        .unwrap();
    assert_eq!(alice.peer_btc_pubkey(), Some(ctx(0x22, 0).btc_pubkey));
    assert_eq!(bob.peer_btc_pubkey(), Some(ctx(0x11, 0).btc_pubkey));

    // Alice funds NIM → FundingProof → Bob observes.
    let nim_fp = alice.fund(head, vec![0x11; 248], [0xC1; 32]).unwrap();
    bob.recv_funding_proof(&wire(SwapKind::FundingProof, &nim_fp))
        .unwrap();

    // Bob funds BTC → FundingProof → Alice observes (BothFunded on both sides).
    let btc_fp = bob.fund(head, vec![0x22; 120], [0xC2; 32]).unwrap();
    alice
        .recv_funding_proof(&wire(SwapKind::FundingProof, &btc_fp))
        .unwrap();
    assert_eq!(alice.phase(), SwapPhase::BothFunded);
    assert_eq!(bob.phase(), SwapPhase::BothFunded);

    // Alice claims BTC (reveals S) → PreimageReveal → Bob reads S, claims NIM.
    let reveal = alice.claim_and_reveal(secret.to_vec(), [0xC3; 32]).unwrap();
    let learned = bob
        .recv_reveal(&wire(SwapKind::PreimageReveal, &reveal), secret)
        .unwrap();
    assert_eq!(learned, secret);

    // Both settle.
    alice.settle().unwrap();
    bob.settle().unwrap();
    assert_eq!(alice.phase(), SwapPhase::Settled);
    assert_eq!(bob.phase(), SwapPhase::Settled);
}

#[test]
fn a_reveal_with_the_wrong_secret_is_rejected() {
    let head = 0;
    let p = LadderParams::default();
    let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], p);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);
    let accept = bob.recv_propose(&signed(&propose, 0xA1), head).unwrap();
    alice.recv_accept(&accept, head).unwrap();
    let nim_fp = alice.fund(head, vec![0x11; 248], [0xC1; 32]).unwrap();
    bob.recv_funding_proof(&nim_fp).unwrap();
    let btc_fp = bob.fund(head, vec![0x22; 120], [0xC2; 32]).unwrap();
    alice.recv_funding_proof(&btc_fp).unwrap();
    let reveal = alice.claim_and_reveal(vec![0u8; 32], [0xC3; 32]).unwrap();
    // A secret that doesn't open the hashlock is refused — bob does not advance.
    assert_eq!(
        bob.recv_reveal(&reveal, [0x99u8; 32]),
        Err(CoordError::BadPreimage)
    );
    assert_eq!(bob.phase(), SwapPhase::BothFunded);
}

#[test]
fn out_of_order_funding_proof_before_accept_is_rejected() {
    // A FundingProof arrives before bob has even accepted — rejected, state unchanged.
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let fp = tx_envelope([0x7A; SWAP_ID_LEN], SwapLegId::Nim, vec![0u8; 8], [0; 32]);
    assert!(matches!(
        bob.recv_funding_proof(&fp),
        Err(CoordError::Swap(SwapError::IllegalTransition { .. }))
    ));
    assert_eq!(bob.phase(), SwapPhase::Proposed);
}

#[test]
fn duplicate_accept_is_rejected_without_corrupting_state() {
    let (head, p) = (0, LadderParams::default());
    let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], p);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);
    let accept = bob.recv_propose(&signed(&propose, 0xA1), head).unwrap();
    alice.recv_accept(&accept, head).unwrap();
    let learned = alice.peer_btc_pubkey();
    // A replayed Accept is rejected (already Accepted) and changes nothing.
    assert!(matches!(
        alice.recv_accept(&accept, head),
        Err(CoordError::Swap(SwapError::IllegalTransition { .. }))
    ));
    assert_eq!(alice.phase(), SwapPhase::Accepted);
    assert_eq!(alice.peer_btc_pubkey(), learned);
}

#[test]
fn an_unsafe_ladder_refuses_to_accept_and_learns_nothing() {
    // margin = 10000 - 5000 = 5000; require 6000 → MarginTooThin → refuse to fund this swap.
    let strict = LadderParams {
        delta_safe_blocks: 6_000,
        min_claim_window_blocks: 1_800,
    };
    let (_alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], strict);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), strict);
    // The Propose is authentic — it is the unsafe LADDER, not authentication, that must reject it.
    assert!(matches!(
        bob.recv_propose(&signed(&propose, 0xA1), 0),
        Err(CoordError::Swap(SwapError::UnsafeLadder(_)))
    ));
    assert_eq!(bob.phase(), SwapPhase::Proposed); // not accepted
    assert_eq!(bob.peer_btc_pubkey(), None); // hardening: nothing learned on a failed accept
}

#[test]
fn a_propose_for_a_different_swap_is_rejected() {
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let mut other = ctx(0x11, 0xA1);
    other.swap_id = [0xFF; SWAP_ID_LEN]; // a Propose for some other swap
    let (_a, propose) = SwapCoordinator::new_initiator(other, [42u8; 32], LadderParams::default());
    // Authentic, but for a different swap_id → rejected on the swap-mismatch check, not authentication.
    assert!(matches!(
        bob.recv_propose(&signed(&propose, 0xA1), 0),
        Err(CoordError::BadMessage { .. })
    ));
    assert_eq!(bob.phase(), SwapPhase::Proposed);
}

// --- S2 / #73: the authenticated-Propose gate (recv_propose rejects an inauthentic Propose) --------

#[test]
fn recv_propose_rejects_an_unsigned_proposal() {
    // An unsigned Propose carries no pubkey/signature → the responder refuses it and never spins up a
    // coordinator on unauthenticated terms (a relay could otherwise inject one).
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let (_a, propose) =
        SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], LadderParams::default());
    assert_eq!(
        bob.recv_propose(&propose, 0),
        Err(CoordError::UnauthenticProposal)
    );
    assert_eq!(bob.phase(), SwapPhase::Proposed); // untouched
    assert_eq!(bob.peer_btc_pubkey(), None); // learned nothing
}

#[test]
fn recv_propose_rejects_a_tampered_proposal() {
    // A relay signs a valid Propose, then rewrites a term on the wire (give_amount) but keeps the old
    // signature — the signature no longer covers the terms, so the responder rejects it.
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let (_a, propose) =
        SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], LadderParams::default());
    let mut tampered = signed(&propose, 0xA1);
    tampered.give_amount = Some(1);
    let on_wire = wire(SwapKind::Propose, &tampered);
    assert_eq!(
        bob.recv_propose(&on_wire, 0),
        Err(CoordError::UnauthenticProposal)
    );
    assert_eq!(bob.phase(), SwapPhase::Proposed);
}

#[test]
fn recv_propose_rejects_a_forged_proposal() {
    // MITM: an attacker re-signs the Propose under ITS OWN key. That key does not hash to the
    // Propose's `nim_address` (the self-certifying bind), so the responder rejects it — a relay can
    // sign over tampered terms, but only under a key it controls, which fails the address check.
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let (_a, propose) =
        SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], LadderParams::default());
    // Signed under seed 0x99, whose address ≠ the Propose's nim_address (owned by 0xA1).
    let forged = signed(&propose, 0x99);
    assert_eq!(
        bob.recv_propose(&forged, 0),
        Err(CoordError::UnauthenticProposal)
    );
    assert_eq!(bob.phase(), SwapPhase::Proposed);
    assert_eq!(bob.peer_btc_pubkey(), None);
}

#[test]
fn recv_propose_accepts_a_valid_signed_proposal() {
    // The honest path: a Propose signed by the NIM key owning its `nim_address` authenticates, so the
    // responder accepts, learns the initiator's BTC pubkey, and reaches Accepted.
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), LadderParams::default());
    let (_a, propose) =
        SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], LadderParams::default());
    let accept = bob
        .recv_propose(&wire(SwapKind::Propose, &signed(&propose, 0xA1)), 0)
        .expect("a valid signed Propose is accepted");
    assert_eq!(accept.swap_id, [0x7A; SWAP_ID_LEN]);
    assert_eq!(bob.phase(), SwapPhase::Accepted);
    assert_eq!(bob.peer_btc_pubkey(), Some(ctx(0x11, 0).btc_pubkey));
}

// --- S1 / #72: the funding-verification gate (never fund or reveal on a message alone) -------------

/// A responder that has accepted the Propose but funded nothing — the moment before the S1 theft.
fn responder_at_accepted() -> SwapCoordinator {
    let p = LadderParams::default();
    let (_alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], p);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);
    bob.recv_propose(&signed(&propose, 0xA1), 0).unwrap();
    assert_eq!(bob.phase(), SwapPhase::Accepted);
    bob
}

/// An initiator that has funded NIM and now must NOT reveal `S` until it has verified the
/// responder's BTC HTLC on-chain.
fn initiator_at_self_funded() -> SwapCoordinator {
    let p = LadderParams::default();
    let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], p);
    let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);
    let accept = bob.recv_propose(&signed(&propose, 0xA1), 0).unwrap();
    alice.recv_accept(&accept, 0).unwrap();
    alice.fund(0, vec![0x11; 248], [0xC1; 32]).unwrap();
    assert_eq!(alice.phase(), SwapPhase::SelfFunded);
    alice
}

#[test]
fn a_responder_refuses_to_fund_when_the_nim_htlc_is_absent() {
    use crate::swap_funding_verify::{FundingObservation, SimVerifier};
    let mut bob = responder_at_accepted();
    let v = SimVerifier::returning(FundingObservation::Absent);
    assert!(matches!(
        bob.verify_and_observe_funding(&v, 1),
        Err(CoordError::FundingUnverified(FundingRejected::NotFundedYet))
    ));
    // The anti-theft invariant: it did NOT reach InitiatorFunded, so the node never funds BTC.
    assert_eq!(bob.phase(), SwapPhase::Accepted);
}

#[test]
fn a_responder_refuses_an_underfunded_nim_htlc() {
    use crate::swap_funding_verify::{FundingObservation, SimVerifier};
    let mut bob = responder_at_accepted();
    // give_amount is 100_000; an HTLC that only locks 1 luna is the classic lie.
    let v = SimVerifier::returning(FundingObservation::Found {
        amount: 1,
        timeout: 10_000,
        confirmations: 9,
    });
    assert!(matches!(
        bob.verify_and_observe_funding(&v, 1),
        Err(CoordError::FundingUnverified(
            FundingRejected::Underfunded { .. }
        ))
    ));
    assert_eq!(bob.phase(), SwapPhase::Accepted);
}

#[test]
fn a_responder_refuses_a_wrong_hashlock() {
    use crate::swap_funding_verify::{FundingObservation, MismatchReason, SimVerifier};
    let mut bob = responder_at_accepted();
    let v = SimVerifier::returning(FundingObservation::Mismatch(MismatchReason::Hashlock));
    assert!(matches!(
        bob.verify_and_observe_funding(&v, 1),
        Err(CoordError::FundingUnverified(FundingRejected::Mismatch(
            MismatchReason::Hashlock
        )))
    ));
    assert_eq!(bob.phase(), SwapPhase::Accepted);
}

#[test]
fn a_responder_refuses_a_shallow_nim_htlc_until_it_is_buried() {
    use crate::swap_funding_verify::{FundingObservation, SimVerifier};
    let mut bob = responder_at_accepted();
    let v = SimVerifier::returning(FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 1,
    });
    assert!(matches!(
        bob.verify_and_observe_funding(&v, 6),
        Err(CoordError::FundingUnverified(
            FundingRejected::TooShallow { .. }
        ))
    ));
    assert_eq!(bob.phase(), SwapPhase::Accepted);
}

#[test]
fn a_responder_funds_only_once_the_nim_htlc_is_verified() {
    use crate::swap_funding_verify::SimVerifier;
    let mut bob = responder_at_accepted();
    let v = SimVerifier::healthy(100_000, 10_000, 6);
    bob.verify_and_observe_funding(&v, 3).unwrap();
    assert_eq!(bob.phase(), SwapPhase::InitiatorFunded); // now — and only now — may it fund BTC
}

#[test]
fn an_initiator_will_not_reveal_until_the_btc_htlc_is_verified() {
    use crate::swap_funding_verify::{FundingObservation, SimVerifier};
    let mut alice = initiator_at_self_funded();
    // No BTC HTLC on-chain → refuse to advance to BothFunded, so `S` is never revealed.
    let absent = SimVerifier::returning(FundingObservation::Absent);
    assert!(matches!(
        alice.verify_and_observe_funding(&absent, 1),
        Err(CoordError::FundingUnverified(_))
    ));
    assert_eq!(alice.phase(), SwapPhase::SelfFunded);
    // Once the responder's BTC HTLC is verified (take_amount 50_000, T_B 5_000) → BothFunded.
    let ok = SimVerifier::healthy(50_000, 5_000, 6);
    alice.verify_and_observe_funding(&ok, 3).unwrap();
    assert_eq!(alice.phase(), SwapPhase::BothFunded);
}

#[test]
fn a_responder_advances_only_when_the_nim_htlc_appears_on_the_ledger() {
    // The end-to-end anti-theft property against a chain oracle (not a message): the responder
    // funds its BTC ONLY after the initiator's NIM HTLC is really on-chain, buried deep enough.
    use crate::swap_funding_verify::{LedgerVerifier, OnChainHtlc};
    let mut bob = responder_at_accepted();
    let mut chain = LedgerVerifier::new();

    // Empty chain → not funded yet; stays put (never funds BTC on the initiator's word alone).
    assert!(matches!(
        bob.verify_and_observe_funding(&chain, 3),
        Err(CoordError::FundingUnverified(FundingRejected::NotFundedYet))
    ));
    assert_eq!(bob.phase(), SwapPhase::Accepted);

    // The initiator's NIM HTLC appears (pays bob's nim_address 0xB2, agreed hashlock) but only 1
    // block deep → too shallow, still refuse.
    let nim_htlc = |confs| OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock: sha256(&[42u8; 32]),
        recipient: nim_address(0xB2).to_vec(),
        amount: 100_000,
        timeout: 10_000,
        confirmations: confs,
    };
    chain.fund(nim_htlc(1));
    assert!(matches!(
        bob.verify_and_observe_funding(&chain, 3),
        Err(CoordError::FundingUnverified(
            FundingRejected::TooShallow { .. }
        ))
    ));
    assert_eq!(bob.phase(), SwapPhase::Accepted);

    // Buried to depth 3 → the responder finally advances (and only now would it fund BTC).
    chain.fund(nim_htlc(3));
    bob.verify_and_observe_funding(&chain, 3).unwrap();
    assert_eq!(bob.phase(), SwapPhase::InitiatorFunded);
}
