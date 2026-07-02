//! # swap_usdc_e2e_tests — the NIM⇄USDC atomicity proof (P2, `polygon-leg` + `cfg(test)`)
//!
//! The exact atomicity argument `swap_e2e_tests` makes for NIM⇄BTC, but with the counterparty leg
//! being a [`crate::swap_usdc_leg::UsdcLeg`] — the Polygon HTLC contract model — instead of a second
//! `MockLeg`. It drives both [`crate::swap::Swap`] state machines (Alice the NIM initiator, Bob the
//! USDC responder) through the happy path and the adversarial paths, asserting the one invariant that
//! answers "could someone lose funds?": **no one-sided settlement** — either both legs are claimed or
//! neither is. The same secret `S` that claims the USDC leg is read off that claim to settle the NIM
//! leg; the worst case is a clean two-sided refund.
//!
//! Because the USDC leg implements the SAME [`crate::swap_leg::SwapLeg`] trait and shares the SHA-256
//! hashlock, the engine is unchanged — this is what "the swap engine is chain-agnostic" means in
//! practice. Sim only: no EVM tx-building, no RPC, no funds (P3/P4, gated).

use crate::swap::{LadderParams, Swap, SwapAction, SwapError, SwapPhase, SwapRole, SwapTerms};
use crate::swap_leg::{sha256, HtlcParams, LegOutcome, MockLeg, SwapLeg};
use crate::swap_usdc_leg::{UsdcLeg, MICRO_USDC};
use crate::swap_wire::SwapLegId;

const GIVE_NIM: u64 = 100_000; // luna Alice locks on the NIM leg
const TAKE_USDC: u64 = 25 * MICRO_USDC; // 25.000000 USDC Bob locks on the Polygon leg
const T_A: u64 = 10_000; // NIM-leg timeout (longer)
const T_B: u64 = 5_000; // USDC-leg timeout (shorter); margin 5000 ≥ Δ_safe, window 5000 ≥ min

// Bob's EVM accounts: he funds the USDC escrow (sender) and Alice claims it (receiver).
const BOB_EVM: [u8; 20] = [0xB0; 20];
const ALICE_EVM: [u8; 20] = [0xA1; 20];

fn terms() -> SwapTerms {
    SwapTerms {
        nim_timeout: T_A,
        counterparty_timeout: T_B,
    }
}

/// THE invariant, leg-type-agnostic: after a scenario fully plays out, no leg may be left
/// funded-and-unresolved, and the two legs must agree — both claimed, or neither. A single claimed
/// leg with the other stuck would be a one-sided settlement (a theft); this forbids it.
fn assert_no_one_sided(nim: LegOutcome, usdc: LegOutcome) {
    assert_ne!(nim, LegOutcome::Funded, "NIM leg left unresolved");
    assert_ne!(usdc, LegOutcome::Funded, "USDC leg left unresolved");
    let nim_claimed = nim == LegOutcome::Claimed;
    let usdc_claimed = usdc == LegOutcome::Claimed;
    assert_eq!(
        nim_claimed, usdc_claimed,
        "ONE-SIDED SETTLEMENT: nim_claimed={nim_claimed} usdc_claimed={usdc_claimed}"
    );
}

/// Both parties accept + both fund, leaving the swap at `BothFunded` with both legs locked.
/// Returns (alice, bob, nim_leg, usdc_leg, secret).
fn fund_both() -> (Swap, Swap, MockLeg, UsdcLeg, [u8; 32]) {
    let p = LadderParams::default();
    let secret = [42u8; 32];
    let hashlock = sha256(&secret);
    let head = 0;

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    let mut bob = Swap::new(SwapRole::Responder, terms());
    let mut nim_leg = MockLeg::new();
    let mut usdc_leg = UsdcLeg::new(BOB_EVM, ALICE_EVM); // Bob funds, Alice claims

    alice.accept(head, &p).unwrap();
    bob.accept(head, &p).unwrap();

    // Alice funds the NIM leg first.
    let act = alice.fund(head, &p).unwrap();
    assert_eq!(
        act,
        SwapAction::FundLeg {
            leg: SwapLegId::Nim,
            timeout: T_A
        }
    );
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();

    // Bob sees the initiator's funding, then funds the USDC leg on Polygon.
    bob.observe_initiator_funded().unwrap();
    let act = bob.fund(head, &p).unwrap();
    assert_eq!(
        act,
        SwapAction::FundLeg {
            leg: SwapLegId::Counterparty,
            timeout: T_B
        }
    );
    usdc_leg
        .fund(HtlcParams {
            hashlock,
            amount: TAKE_USDC,
            timeout: T_B,
        })
        .unwrap();
    assert!(usdc_leg.swap_id().is_some()); // the contract assigned an on-chain slot

    // Both observe both legs funded.
    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();
    assert_eq!(alice.phase, SwapPhase::BothFunded);
    assert_eq!(bob.phase, SwapPhase::BothFunded);

    (alice, bob, nim_leg, usdc_leg, secret)
}

#[test]
fn happy_path_both_legs_settle() {
    let (mut alice, mut bob, mut nim_leg, mut usdc_leg, secret) = fund_both();
    let head = 100; // well before either timeout

    // Alice claims the USDC leg, revealing the secret on Polygon.
    assert_eq!(
        alice.reveal_and_claim().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    usdc_leg.claim(secret, head).unwrap();
    alice.observe_settled().unwrap();

    // Bob reads the secret off the USDC `withdraw` (as he would off-chain or over the mesh)...
    let learned = usdc_leg
        .revealed_preimage()
        .expect("secret is public after the claim");
    assert_eq!(learned, secret);
    // ...and claims the NIM leg with it.
    assert_eq!(
        bob.observe_secret().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.claim(learned, head).unwrap();
    bob.observe_settled().unwrap();

    assert_eq!(alice.phase, SwapPhase::Settled);
    assert_eq!(bob.phase, SwapPhase::Settled);
    assert_eq!(nim_leg.outcome(), LegOutcome::Claimed); // Bob got the NIM
    assert_eq!(usdc_leg.outcome(), LegOutcome::Claimed); // Alice got the USDC
    assert_no_one_sided(nim_leg.outcome(), usdc_leg.outcome());
}

#[test]
fn responder_never_funds_initiator_refunds() {
    let p = LadderParams::default();
    let secret = [7u8; 32];
    let hashlock = sha256(&secret);

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    alice.accept(0, &p).unwrap();
    alice.fund(0, &p).unwrap();
    let mut nim_leg = MockLeg::new();
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();
    // Bob never funds — there is no USDC leg, so Alice can never claim anything.
    let usdc_leg = UsdcLeg::new(BOB_EVM, ALICE_EVM);

    // Alice can't get her USDC; once `T_A` passes she refunds her NIM. No loss.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(alice.phase, SwapPhase::Refunded);
    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(usdc_leg.outcome(), LegOutcome::Unfunded);
    assert_no_one_sided(nim_leg.outcome(), usdc_leg.outcome());
}

#[test]
fn initiator_vanishes_after_funding_both_refund() {
    let (mut alice, _bob, mut nim_leg, mut usdc_leg, _secret) = fund_both();

    // Alice never reveals/claims. Bob's USDC leg has the shorter timeout, so he refunds first...
    usdc_leg.refund(T_B + 1).unwrap();
    // ...then Alice's NIM leg passes its timeout and she refunds too.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(usdc_leg.outcome(), LegOutcome::Refunded);
    assert_no_one_sided(nim_leg.outcome(), usdc_leg.outcome()); // neither claimed — unwound cleanly
}

#[test]
fn unsafe_ladder_refuses_to_fund_so_nothing_is_at_risk() {
    let p = LadderParams::default();
    // A thin-margin ladder: T_A - T_B = 1000 < Δ_safe (3600).
    let bad = SwapTerms {
        nim_timeout: 6_000,
        counterparty_timeout: 5_000,
    };
    let mut alice = Swap::new(SwapRole::Initiator, bad);
    assert!(matches!(
        alice.accept(0, &p),
        Err(SwapError::UnsafeLadder(_))
    ));
    // No funding ever happened, so both legs are untouched — trivially safe.
    let (nim_leg, usdc_leg) = (MockLeg::new(), UsdcLeg::new(BOB_EVM, ALICE_EVM));
    assert_eq!(alice.phase, SwapPhase::Proposed);
    assert_no_one_sided(nim_leg.outcome(), usdc_leg.outcome());
}

#[test]
fn a_relay_with_the_wrong_secret_cannot_steal_the_usdc_leg() {
    // A blind relay (or any attacker) sees the funded legs flooding past but does NOT know the
    // secret. It cannot `withdraw` — the SHA-256 hashlock holds — so the funds stay safe.
    let (_alice, _bob, _nim_leg, mut usdc_leg, _secret) = fund_both();
    let attacker_guess = [0xABu8; 32];
    assert!(usdc_leg.claim(attacker_guess, 100).is_err());
    assert_eq!(usdc_leg.outcome(), LegOutcome::Funded); // untouched — no theft
                                                        // Still refundable by Bob after the timeout: no loss.
    usdc_leg.refund(T_B + 1).unwrap();
    assert_eq!(usdc_leg.outcome(), LegOutcome::Refunded);
}

#[test]
fn responder_cannot_claim_before_the_secret_is_revealed() {
    // Liveness/ordering: Bob (responder) only reaches a claimable state via `observe_secret`, and
    // even then needs the actual preimage — he can't jump the gun and claim NIM early.
    let (_alice, mut bob, _nim_leg, _usdc_leg, _secret) = fund_both();
    let mut nim_leg = MockLeg::new();
    nim_leg
        .fund(HtlcParams {
            hashlock: sha256(&[42u8; 32]),
            amount: GIVE_NIM,
            timeout: T_A,
        })
        .unwrap();
    bob.observe_secret().unwrap(); // state-machine transition
    assert!(nim_leg.claim([0u8; 32], 100).is_err()); // but without the real secret, no claim
    assert_eq!(nim_leg.outcome(), LegOutcome::Funded);
}
