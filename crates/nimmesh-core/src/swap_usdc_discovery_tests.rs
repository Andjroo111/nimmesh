//! # swap_usdc_discovery_tests — matched NIM⇄USDC discovery drives a UsdcLeg settlement (P6, `cfg(test)`)
//!
//! Connects the two USDC halves: the P5 discovery layer ([`crate::swap_intent`]) and the P2
//! settlement leg ([`crate::swap_usdc_leg::UsdcLeg`]). A NIM-giver and a USDC-giver [`SwapIntent`]
//! that **match** (`would_initiate_against` + `amount_compatible`) are turned into swap terms exactly
//! as `swap_node::initiate_from_intent` does, the counterparty leg is selected from the matched
//! `counter_asset` via [`crate::swap_leg_select::counterparty_leg_for`] (→ `Usdc`), and the
//! chain-agnostic [`crate::swap::Swap`] state machine is driven with a `UsdcLeg` counterparty through
//! the happy path AND a refund path — proving leg selection by `counter_asset` yields a UsdcLeg and
//! settles atomically (no one-sided settlement). SIM only — money-path gated (no real funds, no EVM
//! broadcast; the EVM addresses here are placeholders until P7 carries them in the intent).

use crate::swap::{LadderParams, Swap, SwapAction, SwapPhase, SwapRole, SwapTerms};
use crate::swap_intent::{Asset, SwapIntent, INTENT_PUBKEY_LEN, INTENT_SIG_LEN};
use crate::swap_leg::{sha256, HtlcParams, LegOutcome, MockLeg, SwapLeg};
use crate::swap_leg_select::{counterparty_leg_for, CounterpartyLeg};
use crate::swap_usdc_leg::{UsdcLeg, MICRO_USDC};
use crate::swap_wire::{SwapLegId, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};

// Sim timelocks — the same fixed values `swap_node::initiate_from_intent` uses (safe at head 0).
const T_A: u64 = 10_000; // NIM leg (longer)
const T_B: u64 = 5_000; // USDC leg (shorter)

// Placeholder EVM accounts for the USDC escrow (P7 carries the real payout address in the intent).
const USDC_GIVER_EVM: [u8; 20] = [0xB0; 20]; // funds the USDC escrow
const NIM_GIVER_EVM: [u8; 20] = [0xA1; 20]; // claims the USDC escrow

/// A discovery intent. `counter_usdc` is the counterparty amount in micro-USDC (the `btc_amount`
/// field carries it when `counter_asset == Usdc`).
fn intent(gives: Asset, counter_asset: Asset, nim: u64, counter_usdc: u64) -> SwapIntent {
    SwapIntent {
        gives,
        counter_asset,
        nim_amount: nim,
        btc_amount: counter_usdc,
        expiry_height: 1_000_000,
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0x07; INTENT_PUBKEY_LEN],
        nim_address: [0xA1; NIM_ADDRESS_LEN],
        btc_pubkey: {
            let mut k = [0x11; BTC_PUBKEY_LEN];
            k[0] = 0x02;
            k
        },
        btc_address: b"unused-for-usdc".to_vec(),
        network_id: 5,
        signature: [0u8; INTENT_SIG_LEN],
    }
}

/// THE invariant (leg-type-agnostic): after a scenario plays out, neither leg is left funded, and the
/// two legs agree — both claimed or neither. A single claimed leg would be a one-sided settlement.
fn assert_no_one_sided(nim: LegOutcome, usdc: LegOutcome) {
    assert_ne!(nim, LegOutcome::Funded, "NIM leg left unresolved");
    assert_ne!(usdc, LegOutcome::Funded, "USDC leg left unresolved");
    assert_eq!(
        nim == LegOutcome::Claimed,
        usdc == LegOutcome::Claimed,
        "ONE-SIDED SETTLEMENT"
    );
}

/// A matched NIM-giver (wants USDC) + USDC-giver pair, plus the terms/amounts derived from them the
/// way `initiate_from_intent` does, and the selected counterparty leg. Asserts the match + that
/// selection yields the USDC leg.
struct Matched {
    terms: SwapTerms,
    give_nim: u64,
    take_usdc: u64,
}

fn matched_usdc_pair() -> Matched {
    // A NIM-giver offering 200_000 NIM for 100 USDC; a USDC-giver asking only 180_000 NIM (a better
    // rate) for the same 100 USDC → the rates cross, so they match.
    let standing = intent(Asset::Nim, Asset::Usdc, 200_000, 100 * MICRO_USDC);
    let incoming = intent(Asset::Usdc, Asset::Usdc, 180_000, 100 * MICRO_USDC);

    assert!(
        standing.would_initiate_against(&incoming),
        "the NIM-giver should initiate against the crossing USDC-giver"
    );
    assert!(standing.amount_compatible(&incoming));
    // Leg selection from the matched counter_asset → the USDC leg (not BTC).
    assert_eq!(
        counterparty_leg_for(standing.counter_asset),
        Some(CounterpartyLeg::Usdc)
    );

    Matched {
        // Same fixed sim timelocks as initiate_from_intent.
        terms: SwapTerms {
            nim_timeout: T_A,
            counterparty_timeout: T_B,
        },
        // The swap executes at the INITIATOR's amounts.
        give_nim: standing.nim_amount,
        take_usdc: standing.btc_amount,
    }
}

/// Both parties accept + fund, leaving the swap at `BothFunded` with the NIM (MockLeg) + USDC
/// (UsdcLeg) escrows locked. Returns (alice, bob, nim_leg, usdc_leg, secret).
fn fund_both(m: &Matched) -> (Swap, Swap, MockLeg, UsdcLeg, [u8; 32]) {
    let p = LadderParams::default();
    let secret = [42u8; 32];
    let hashlock = sha256(&secret);

    let mut alice = Swap::new(SwapRole::Initiator, m.terms);
    let mut bob = Swap::new(SwapRole::Responder, m.terms);
    let mut nim_leg = MockLeg::new();
    let mut usdc_leg = UsdcLeg::new(USDC_GIVER_EVM, NIM_GIVER_EVM);

    alice.accept(0, &p).unwrap();
    bob.accept(0, &p).unwrap();

    // Alice funds the NIM leg.
    assert_eq!(
        alice.fund(0, &p).unwrap(),
        SwapAction::FundLeg {
            leg: SwapLegId::Nim,
            timeout: T_A
        }
    );
    nim_leg
        .fund(HtlcParams {
            hashlock,
            amount: m.give_nim,
            timeout: T_A,
        })
        .unwrap();

    // Bob funds the USDC leg.
    bob.observe_initiator_funded().unwrap();
    assert_eq!(
        bob.fund(0, &p).unwrap(),
        SwapAction::FundLeg {
            leg: SwapLegId::Counterparty,
            timeout: T_B
        }
    );
    usdc_leg
        .fund(HtlcParams {
            hashlock,
            amount: m.take_usdc,
            timeout: T_B,
        })
        .unwrap();

    alice.observe_counterparty_funded().unwrap();
    bob.observe_counterparty_funded().unwrap();
    assert_eq!(alice.phase, SwapPhase::BothFunded);
    assert_eq!(bob.phase, SwapPhase::BothFunded);
    (alice, bob, nim_leg, usdc_leg, secret)
}

#[test]
fn a_matched_usdc_intent_pair_settles_atomically_via_the_usdc_leg() {
    let m = matched_usdc_pair();
    let (mut alice, mut bob, mut nim_leg, mut usdc_leg, secret) = fund_both(&m);
    let head = 100;

    // Alice claims the USDC leg, revealing S on Polygon.
    assert_eq!(
        alice.reveal_and_claim().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    usdc_leg.claim(secret, head).unwrap();
    alice.observe_settled().unwrap();

    // Bob reads S off the USDC claim and claims the NIM leg.
    let learned = usdc_leg.revealed_preimage().expect("S public after claim");
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
    assert_eq!(usdc_leg.amount(), Some(m.take_usdc)); // the discovered USDC amount got escrowed
    assert_no_one_sided(nim_leg.outcome(), usdc_leg.outcome());
}

#[test]
fn a_matched_usdc_intent_pair_unwinds_to_a_clean_refund() {
    let m = matched_usdc_pair();
    let (mut alice, _bob, mut nim_leg, mut usdc_leg, _secret) = fund_both(&m);

    // Alice never reveals/claims. Bob's USDC leg (shorter timeout) refunds first, then Alice's NIM.
    usdc_leg.refund(T_B + 1).unwrap();
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
