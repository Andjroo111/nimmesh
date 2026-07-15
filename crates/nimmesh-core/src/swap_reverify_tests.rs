//! # swap_reverify_tests — the tick-driven counterparty-funding re-verification (the field-stall fix)
//! plus the verify-verdict diagnostics recording. Kept a sibling of `swap_session_tests` (own local
//! fixtures) so both stay under the 800-line ceiling.
//!
//! The stall these prove out: a responder verifies the initiator's NIM HTLC ONLY when a `FundingProof`
//! message arrives; that proof lands seconds after funding, when the HTLC is still shallow (1-2 < the
//! mainnet policy's 10), so the message-time verify correctly fails `TooShallow`. Recovery then depended
//! on a RETRANSMITTED proof arriving after the depth was reached — but retransmits are TTL-bounded
//! (RETRANSMIT_TTL = 32), so once they ran out the swap stalled forever. The fix re-runs the SAME
//! fail-closed gate on every maintenance tick, so the swap advances the instant its counterparty HTLC
//! reaches its per-chain depth, with no dependence on a retransmit.

use crate::swap::{LadderParams, SwapPhase, SwapTerms};
use crate::swap_coordinator::{SwapContext, SwapCoordinator};
use crate::swap_funding_verify::{
    ConfirmationPolicy, FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_intent::Asset;
use crate::swap_leg::sha256;
use crate::swap_messages::SwapProposal;
use crate::swap_rate::RatePolicy;
use crate::swap_session::*;
use crate::swap_wire::{
    encode_swap, SwapEnvelope, SwapKind, SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN, SWAP_ID_LEN,
};
use std::sync::{Arc, Mutex};

// --- local fixtures (mirrors swap_session_tests; each `swap_*_tests` file carries its own) ---------

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

fn only(out: Vec<(crate::packet::MessageType, Vec<u8>)>) -> (crate::packet::MessageType, Vec<u8>) {
    assert_eq!(out.len(), 1);
    out.into_iter().next().unwrap()
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

/// A funding verifier whose single observation can be MUTATED between ticks — the deterministic
/// (no wall-clock, ADR-0005) stand-in for a chain whose confirmation depth climbs. Ignores the
/// expectation, like `SimVerifier`, but the test can grow the depth in place.
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
    fn observe(&self, _expect: &HtlcExpectation) -> FundingObservation {
        self.0.lock().unwrap().clone()
    }
}

/// A responder session with the mainnet-like NIM depth (10) + USDC counterparty, wired to `verifier`.
fn responder_with(verifier: MutVerifier) -> SwapSession {
    SwapSession::new(identity(0x22), LadderParams::default())
        .with_counterparty_chain(Asset::Usdc)
        .with_confirmation_policy(
            ConfirmationPolicy::testnet_defaults()
                .with_nim(10)
                .with_usdc(64),
        )
        .with_funding_verifier(Box::new(verifier))
}

#[test]
fn a_tick_advances_a_responder_once_the_nim_leg_reaches_depth_without_a_retransmit() {
    // The exact field stall: the initiator's NIM HTLC is on-chain but shallow (3 < 10) when the
    // FundingProof arrives, so the message-time verify correctly fails TooShallow and the responder
    // stays at Accepted. It must NOT need a retransmitted proof to advance — the next maintenance tick
    // re-consults the chain, sees depth 10, and advances it, with no message in between.
    let head = 0;
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();
    let nim_fp = nim_funding_proof(swap_id);

    let chain = MutVerifier::new(FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 3,
    });
    let mut bob = responder_with(chain.clone());
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );

    // Proof arrives while shallow → refused, stays Accepted, and the verdict is recorded for the app.
    assert!(matches!(
        bob.on_message(SwapKind::FundingProof, &nim_fp, head),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );

    // A tick BEFORE the depth is reached does nothing (still shallow) — proving the tick isn't a
    // blanket advance; it re-runs the same fail-closed gate.
    assert!(bob.reverify_awaited_funding().is_empty());
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::Accepted
    );

    // The chain buries the HTLC to the policy depth. NEXT tick advances it — no retransmit.
    chain.set(FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 10,
    });
    assert_eq!(bob.reverify_awaited_funding(), vec![swap_id]);
    assert_eq!(
        bob.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::InitiatorFunded
    );
    // Verified swaps stop being re-polled (keeps the shared-IP RPC quiet).
    assert!(bob.reverify_awaited_funding().is_empty());
}

#[test]
fn a_tick_advances_the_initiators_mirror_case_once_the_usdc_leg_reaches_depth() {
    // The mirror stall class: the initiator funded NIM and is at SelfFunded awaiting the responder's
    // USDC leg. USDC needs 64 Polygon blocks (~2 min); the FundingProof lands far shallower, so the
    // initiator must NOT reveal S yet — but the next tick after depth 64 advances it to BothFunded so
    // the driver can reveal.
    let head = 0;
    let swap_id = [0x9B; SWAP_ID_LEN];
    let chain = MutVerifier::new(FundingObservation::Found {
        amount: 50_000,
        timeout: 5_000,
        confirmations: 10,
    });
    let mut alice = SwapSession::new(identity(0x11), LadderParams::default())
        .with_counterparty_chain(Asset::Usdc)
        .with_confirmation_policy(ConfirmationPolicy::testnet_defaults().with_usdc(64))
        .with_funding_verifier(Box::new(chain.clone()));
    let (coord, _propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    alice.add_initiator(swap_id, coord);
    // Accept, then fund NIM → SelfFunded (awaiting the counterparty leg).
    let accept = crate::swap_messages::SwapAcceptance {
        swap_id,
        nim_address: [0xB2; NIM_ADDRESS_LEN],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: [0x03; BTC_PUBKEY_LEN],
    }
    .to_envelope();
    alice
        .on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), head)
        .unwrap();
    alice
        .coordinator(&swap_id)
        .unwrap()
        .fund(head, vec![0x11; 248], [0xC1; 32])
        .unwrap();
    assert_eq!(
        alice.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::SelfFunded
    );

    // The responder's USDC FundingProof, still too shallow (10 < 64) → refused, stays SelfFunded.
    let usdc_fp = encode_swap(&crate::swap_messages::tx_envelope(
        swap_id,
        SwapLegId::Counterparty,
        vec![0x22; 120],
        [0xC2; 32],
    ))
    .unwrap();
    assert!(matches!(
        alice.on_message(SwapKind::FundingProof, &usdc_fp, head),
        Err(SessionError::Coord(_))
    ));
    assert_eq!(
        alice.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::SelfFunded
    );
    assert!(alice.reverify_awaited_funding().is_empty());

    // Buried to 64 → next tick advances to BothFunded (now the driver may reveal S).
    chain.set(FundingObservation::Found {
        amount: 50_000,
        timeout: 5_000,
        confirmations: 64,
    });
    assert_eq!(alice.reverify_awaited_funding(), vec![swap_id]);
    assert_eq!(
        alice.coordinator(&swap_id).unwrap().phase(),
        SwapPhase::BothFunded
    );
}

#[test]
fn a_mismatch_or_absent_hint_refuses_forever_across_ticks() {
    // No weakening: the tick re-verify is the SAME fail-closed gate. An HTLC that is absent, or that
    // pays us under a different hashlock, never advances — not on the first tick, not on the hundredth.
    let head = 0;
    let (_c, propose) = SwapCoordinator::new_initiator(
        ctx([0x7A; SWAP_ID_LEN], 0x11),
        [42u8; 32],
        LadderParams::default(),
    );
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();

    for (obs, swap_id) in [
        (FundingObservation::Absent, [0x7A; SWAP_ID_LEN]),
        (
            FundingObservation::Mismatch(MismatchReason::Hashlock),
            [0x7A; SWAP_ID_LEN],
        ),
    ] {
        let chain = MutVerifier::new(obs);
        let mut bob = responder_with(chain);
        only(
            bob.on_message(SwapKind::Propose, &propose_bytes, head)
                .unwrap(),
        );
        assert!(bob
            .on_message(SwapKind::FundingProof, &nim_funding_proof(swap_id), head)
            .is_err());
        // Many ticks, never an advance — the counterparty leg is not honestly funded to us.
        for _ in 0..128 {
            assert!(bob.reverify_awaited_funding().is_empty());
        }
        assert_eq!(
            bob.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Accepted
        );
    }
}

#[test]
fn the_verify_verdict_is_recorded_for_the_diagnostics_surface() {
    // Deliverable B: every verify attempt records the last verdict + a climbing attempt counter, so
    // the app can render `verify ▸ NIM too shallow 3/10 · attempt N` instead of guessing.
    let head = 0;
    let swap_id = [0x7A; SWAP_ID_LEN];
    let (_c, propose) =
        SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
    let propose_bytes = encode_swap(&signed(&propose, 0x11)).unwrap();

    let chain = MutVerifier::new(FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 3,
    });
    let mut bob = responder_with(chain.clone());
    only(
        bob.on_message(SwapKind::Propose, &propose_bytes, head)
            .unwrap(),
    );
    let _ = bob.on_message(SwapKind::FundingProof, &nim_funding_proof(swap_id), head);

    let note = bob.verify_note(&swap_id).expect("a verdict was recorded");
    assert_eq!(note.text, "NIM too shallow 3/10");
    assert_eq!(note.attempts, 1);

    // A tick that re-checks (still shallow) climbs the attempt counter and keeps the verdict.
    bob.reverify_awaited_funding();
    let note = bob.verify_note(&swap_id).unwrap();
    assert_eq!(note.text, "NIM too shallow 3/10");
    assert_eq!(note.attempts, 2);

    // On success the verdict names the node's next money-path step.
    chain.set(FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 10,
    });
    bob.reverify_awaited_funding();
    assert_eq!(
        bob.verify_note(&swap_id).unwrap().text,
        "verified — funding USDC"
    );
}
