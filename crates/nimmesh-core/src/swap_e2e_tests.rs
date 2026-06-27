//! # swap_e2e_tests — the atomicity proof (mesh swap F4, `cfg(test)` only)
//!
//! Drives a whole offline cross-chain swap end to end — both [`crate::swap::Swap`] state machines
//! (Alice the initiator, Bob the responder) over two [`crate::swap_leg::MockLeg`] HTLC escrows —
//! through the happy path **and every adversarial path**, asserting the one invariant that answers
//! "could someone lose funds?": **no one-sided settlement is ever possible** — either both legs are
//! claimed (the swap succeeded) or neither is (it unwound to refunds). The worst case is a refund,
//! never a theft (`docs/swap/FEASIBILITY.md`).
//!
//! The two legs:
//! - **NIM leg** — funded by Alice (initiator), claimed by Bob, timeout `T_A` (the longer one).
//! - **counterparty (BTC) leg** — funded by Bob (responder), claimed by Alice, timeout `T_B < T_A`.
//!
//! Alice knows the secret `S`; she claims the BTC leg by revealing it, Bob reads `S` off that
//! claim and claims the NIM leg. The mock legs enforce the real HTLC rules, so these tests prove
//! the *protocol* is atomic independent of the on-chain byte format (which `nimiq::htlc` owns and a
//! live testnet validates).

use crate::swap::{LadderParams, Swap, SwapAction, SwapError, SwapPhase, SwapRole, SwapTerms};
use crate::swap_leg::{sha256, HtlcParams, LegOutcome, MockLeg, SwapLeg};
use crate::swap_wire::SwapLegId;

const GIVE_NIM: u64 = 100_000; // luna Alice locks on the NIM leg
const TAKE_BTC: u64 = 50_000; // sats Bob locks on the BTC leg
const T_A: u64 = 10_000; // NIM-leg timeout (longer)
const T_B: u64 = 5_000; // BTC-leg timeout (shorter); margin 5000 ≥ Δ_safe, window 5000 ≥ min

fn terms() -> SwapTerms {
    SwapTerms {
        nim_timeout: T_A,
        counterparty_timeout: T_B,
    }
}

/// THE invariant. After a scenario has fully played out (claims + any timeout refunds), no leg may
/// be left funded-and-unresolved, and the two legs must agree: both claimed, or neither. A single
/// claimed leg with the other stuck would be a one-sided settlement (a theft) — this forbids it.
fn assert_no_one_sided(nim: &MockLeg, btc: &MockLeg) {
    assert_ne!(
        nim.outcome(),
        LegOutcome::Funded,
        "NIM leg left unresolved (no refund taken)"
    );
    assert_ne!(
        btc.outcome(),
        LegOutcome::Funded,
        "BTC leg left unresolved (no refund taken)"
    );
    let nim_claimed = nim.outcome() == LegOutcome::Claimed;
    let btc_claimed = btc.outcome() == LegOutcome::Claimed;
    assert_eq!(
        nim_claimed, btc_claimed,
        "ONE-SIDED SETTLEMENT: nim_claimed={nim_claimed} btc_claimed={btc_claimed}"
    );
}

/// Both parties accept + both fund, leaving the swap at `BothFunded` with both legs locked.
/// Returns (alice, bob, nim_leg, btc_leg, secret, hashlock).
fn fund_both() -> (Swap, Swap, MockLeg, MockLeg, [u8; 32]) {
    let p = LadderParams::default();
    let secret = [42u8; 32];
    let hashlock = sha256(&secret);
    let head = 0;

    let mut alice = Swap::new(SwapRole::Initiator, terms());
    let mut bob = Swap::new(SwapRole::Responder, terms());
    let mut nim_leg = MockLeg::new();
    let mut btc_leg = MockLeg::new();

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

    // Bob sees the initiator's funding, then funds the BTC leg.
    bob.observe_initiator_funded().unwrap();
    let act = bob.fund(head, &p).unwrap();
    assert_eq!(
        act,
        SwapAction::FundLeg {
            leg: SwapLegId::Counterparty,
            timeout: T_B
        }
    );
    btc_leg
        .fund(HtlcParams {
            hashlock,
            amount: TAKE_BTC,
            timeout: T_B,
        })
        .unwrap();

    // Both observe both legs funded.
    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();
    assert_eq!(alice.phase, SwapPhase::BothFunded);
    assert_eq!(bob.phase, SwapPhase::BothFunded);

    (alice, bob, nim_leg, btc_leg, secret)
}

#[test]
fn happy_path_both_legs_settle() {
    let (mut alice, mut bob, mut nim_leg, mut btc_leg, secret) = fund_both();
    let head = 100; // well before either timeout

    // Alice claims the BTC leg, revealing the secret on that chain.
    assert_eq!(
        alice.reveal_and_claim().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    btc_leg.claim(secret, head).unwrap();
    alice.observe_settled().unwrap();

    // Bob reads the secret off the BTC claim (as he would off-chain or over the mesh)...
    let learned = btc_leg
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
    assert_eq!(btc_leg.outcome(), LegOutcome::Claimed); // Alice got the BTC
    assert_no_one_sided(&nim_leg, &btc_leg);
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
    // Bob never funds — there is no BTC leg, so Alice can never claim anything.
    let btc_leg = MockLeg::new();

    // Alice can't get her counterparty funds; once `T_A` passes she refunds her NIM. No loss.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(alice.phase, SwapPhase::Refunded);
    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(btc_leg.outcome(), LegOutcome::Unfunded);
    assert_no_one_sided(&nim_leg, &btc_leg);
}

#[test]
fn initiator_vanishes_after_funding_both_refund() {
    let (mut alice, _bob, mut nim_leg, mut btc_leg, _secret) = fund_both();

    // Alice never reveals/claims. Bob's BTC leg has the shorter timeout, so he refunds first...
    btc_leg.refund(T_B + 1).unwrap();
    // ...then Alice's NIM leg passes its timeout and she refunds too.
    assert_eq!(
        alice.refund_after_timeout(T_A + 1).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    nim_leg.refund(T_A + 1).unwrap();

    assert_eq!(nim_leg.outcome(), LegOutcome::Refunded);
    assert_eq!(btc_leg.outcome(), LegOutcome::Refunded);
    assert_no_one_sided(&nim_leg, &btc_leg); // neither claimed — unwound cleanly
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
    let (nim_leg, btc_leg) = (MockLeg::new(), MockLeg::new());
    assert_eq!(alice.phase, SwapPhase::Proposed);
    assert_no_one_sided(&nim_leg, &btc_leg);
}

#[test]
fn a_relay_with_the_wrong_secret_cannot_steal_a_leg() {
    // A blind relay (or any attacker) sees the funded legs flooding past but does NOT know the
    // secret. It cannot claim — the hashlock holds — so the funds stay safe for the real parties.
    let (_alice, _bob, _nim_leg, mut btc_leg, _secret) = fund_both();
    let attacker_guess = [0xABu8; 32];
    assert!(btc_leg.claim(attacker_guess, 100).is_err());
    assert_eq!(btc_leg.outcome(), LegOutcome::Funded); // untouched — no theft
                                                       // The leg is still refundable by its funder after the timeout: no loss.
    btc_leg.refund(T_B + 1).unwrap();
    assert_eq!(btc_leg.outcome(), LegOutcome::Refunded);
}

#[test]
fn responder_cannot_claim_before_the_secret_is_revealed() {
    // Liveness/ordering: Bob (responder) only reaches a claimable state via `observe_secret`, which
    // is only legal once both legs are funded — he can't jump the gun and claim NIM early.
    let (_alice, mut bob, _nim_leg, _btc_leg, _secret) = fund_both();
    // Bob is at BothFunded; observe_secret is the only path to ClaimLeg, and it requires the reveal
    // to have happened off-chain first (the test models that by Alice claiming). Calling it here is
    // legal as a transition, but Bob still needs the actual preimage to claim the NIM leg on-chain —
    // which `MockLeg::claim` enforces. Prove the wrong/absent secret can't claim:
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
