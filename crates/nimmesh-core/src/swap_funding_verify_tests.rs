//! Tests for `swap_funding_verify`, extracted to a sibling file for the 800-line guard
//! (the established `*_tests.rs` pattern). `super` is `swap_funding_verify`.

use super::*;

#[test]
fn mainnet_confirmation_depths_are_the_reviewed_values() {
    // M7 / ADR-0003 addendum (2026-07-15): the ≤ $5 self-swap FAST-FINALITY depths
    // (NIM 2 / USDC 8-as-fallback / BTC 2) — the USDC verifier's primary burial signal is
    // the Polygon `finalized` tag; this depth-8 only gates an RPC that does not serve it.
    // Never accidentally zero on any chain.
    let m = ConfirmationPolicy::mainnet_defaults();
    assert_eq!(m.required(Asset::Nim), 2);
    assert_eq!(m.required(Asset::Usdc), 8);
    assert_eq!(m.required(Asset::Btc), 2);
    let t = ConfirmationPolicy::testnet_defaults();
    assert!(m.required(Asset::Usdc) > t.required(Asset::Usdc));
    for chain in [Asset::Nim, Asset::Usdc, Asset::Btc] {
        assert!(m.required(chain) > 0);
    }
}

#[test]
fn mainnet_paranoid_restores_the_pre_fast_finality_depths() {
    // The named one-line revert: the old M6 depths (NIM 10 / USDC 64 / BTC 2), strictly
    // at-or-deeper than the fast-finality profile on every chain — never less safe.
    let p = ConfirmationPolicy::mainnet_paranoid();
    assert_eq!(p.required(Asset::Nim), 10);
    assert_eq!(p.required(Asset::Usdc), 64);
    assert_eq!(p.required(Asset::Btc), 2);
    let m = ConfirmationPolicy::mainnet_defaults();
    for chain in [Asset::Nim, Asset::Usdc, Asset::Btc] {
        assert!(p.required(chain) >= m.required(chain));
    }
}

#[test]
fn finalized_confirmations_clears_any_depth_gate() {
    // FINALIZED_CONFIRMATIONS = u32::MAX expresses deterministic finality as "maximally
    // buried": it clears require_funded's depth gate under ANY ConfirmationPolicy without
    // changing the pure safety core (amount/timeout/mismatch rules still apply first).
    let obs = FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: FINALIZED_CONFIRMATIONS,
    };
    for need in [
        ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc),
        ConfirmationPolicy::mainnet_paranoid().required(Asset::Usdc),
        u32::MAX,
    ] {
        assert_eq!(
            require_funded(&obs, &expect(), need),
            Ok(FINALIZED_CONFIRMATIONS)
        );
    }
    // Finality never bypasses the OTHER gates: an underfunded finalized escrow still refuses.
    let under = FundingObservation::Found {
        amount: 1,
        timeout: 10_000,
        confirmations: FINALIZED_CONFIRMATIONS,
    };
    assert!(matches!(
        require_funded(&under, &expect(), 1),
        Err(FundingRejected::Underfunded { .. })
    ));
}

#[test]
fn swap_caps_refuse_above_the_hard_ceiling_per_leg() {
    let caps = SwapCaps::mainnet_first_swap();
    assert_eq!(caps.max_nim_luna, 5_000_000); // 50 NIM
    assert_eq!(caps.max_usdc_micro, 5_000_000); // 5 USDC
    assert_eq!(caps.max_btc_sat, 20_000);
    // At the ceiling is admitted; one unit over any leg is refused.
    assert!(caps.admits(5_000_000, 5_000_000, Asset::Usdc));
    assert!(caps.admits(5_000_000, 20_000, Asset::Btc));
    assert!(!caps.admits(5_000_001, 1_000_000, Asset::Usdc)); // NIM over
    assert!(!caps.admits(100_000, 5_000_001, Asset::Usdc)); // USDC over
    assert!(!caps.admits(100_000, 20_001, Asset::Btc)); // BTC over
}

fn expect() -> HtlcExpectation {
    HtlcExpectation {
        leg: SwapLegId::Nim,
        hashlock: [0x11; HASH_LEN],
        min_amount: 100_000,
        min_timeout: 10_000,
        recipient: vec![0xA1; 20],
        term_anchor: 0,
    }
}

#[test]
fn a_correct_deep_funding_is_accepted() {
    let obs = FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 6,
    };
    assert_eq!(require_funded(&obs, &expect(), 3), Ok(6));
}

#[test]
fn an_overfunded_later_timeout_is_still_accepted() {
    // More amount and a longer timeout than agreed only help us — accept.
    let obs = FundingObservation::Found {
        amount: 250_000,
        timeout: 99_999,
        confirmations: 3,
    };
    assert_eq!(require_funded(&obs, &expect(), 3), Ok(3));
}

#[test]
fn absent_funding_is_not_funded_yet() {
    assert_eq!(
        require_funded(&FundingObservation::Absent, &expect(), 1),
        Err(FundingRejected::NotFundedYet)
    );
}

#[test]
fn a_wrong_hashlock_is_a_hard_mismatch() {
    let obs = FundingObservation::Mismatch(MismatchReason::Hashlock);
    assert_eq!(
        require_funded(&obs, &expect(), 1),
        Err(FundingRejected::Mismatch(MismatchReason::Hashlock))
    );
}

#[test]
fn a_wrong_recipient_is_a_hard_mismatch() {
    let obs = FundingObservation::Mismatch(MismatchReason::Recipient);
    assert_eq!(
        require_funded(&obs, &expect(), 1),
        Err(FundingRejected::Mismatch(MismatchReason::Recipient))
    );
}

#[test]
fn underfunding_is_rejected() {
    // The classic lie: an HTLC that exists but locks less than agreed.
    let obs = FundingObservation::Found {
        amount: 99_999,
        timeout: 10_000,
        confirmations: 6,
    };
    assert_eq!(
        require_funded(&obs, &expect(), 3),
        Err(FundingRejected::Underfunded {
            have: 99_999,
            need: 100_000
        })
    );
}

#[test]
fn a_too_short_timeout_is_rejected() {
    // A shorter timeout than agreed would shrink our claim window below the ladder's assumption.
    let obs = FundingObservation::Found {
        amount: 100_000,
        timeout: 9_999,
        confirmations: 6,
    };
    assert_eq!(
        require_funded(&obs, &expect(), 3),
        Err(FundingRejected::TimeoutTooShort {
            have: 9_999,
            need: 10_000
        })
    );
}

#[test]
fn a_shallow_funding_is_rejected_until_buried() {
    // Reorg safety: a match that is not yet deep enough must wait, not proceed.
    let obs = FundingObservation::Found {
        amount: 100_000,
        timeout: 10_000,
        confirmations: 2,
    };
    assert_eq!(
        require_funded(&obs, &expect(), 3),
        Err(FundingRejected::TooShallow { have: 2, need: 3 })
    );
}

#[test]
fn amount_is_checked_before_depth() {
    // A shallow AND underfunded HTLC surfaces the economic lie first (order is deterministic).
    let obs = FundingObservation::Found {
        amount: 1,
        timeout: 10_000,
        confirmations: 0,
    };
    assert_eq!(
        require_funded(&obs, &expect(), 3),
        Err(FundingRejected::Underfunded {
            have: 1,
            need: 100_000
        })
    );
}

#[test]
fn sim_verifier_healthy_passes_the_gate() {
    let v = SimVerifier::healthy(100_000, 10_000, 6);
    let obs = v.observe(&expect());
    assert_eq!(require_funded(&obs, &expect(), 3), Ok(6));
}

fn on_chain(recipient: Vec<u8>, hashlock: [u8; HASH_LEN], confirmations: u32) -> OnChainHtlc {
    OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock,
        recipient,
        amount: 100_000,
        timeout: 10_000,
        confirmations,
    }
}

#[test]
fn ledger_absent_when_empty() {
    let ledger = LedgerVerifier::new();
    assert_eq!(ledger.observe(&expect()), FundingObservation::Absent);
}

#[test]
fn ledger_finds_our_matching_htlc() {
    let mut ledger = LedgerVerifier::new();
    ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 4));
    assert_eq!(
        ledger.observe(&expect()),
        FundingObservation::Found {
            amount: 100_000,
            timeout: 10_000,
            confirmations: 4
        }
    );
}

#[test]
fn ledger_reports_the_deepest_matching_htlc() {
    // The same HTLC re-published as it is buried deeper — the deepest confirmation wins.
    let mut ledger = LedgerVerifier::new();
    ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 1));
    ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 5));
    assert!(matches!(
        ledger.observe(&expect()),
        FundingObservation::Found {
            confirmations: 5,
            ..
        }
    ));
}

#[test]
fn ledger_flags_an_htlc_that_pays_us_under_the_wrong_hashlock() {
    let mut ledger = LedgerVerifier::new();
    ledger.fund(on_chain(vec![0xA1; 20], [0x99; HASH_LEN], 6)); // pays us, wrong hashlock
    assert_eq!(
        ledger.observe(&expect()),
        FundingObservation::Mismatch(MismatchReason::Hashlock)
    );
}

#[test]
fn ledger_flags_our_hashlock_paying_someone_else() {
    let mut ledger = LedgerVerifier::new();
    ledger.fund(on_chain(vec![0xBE; 20], [0x11; HASH_LEN], 6)); // right hashlock, not our recipient
    assert_eq!(
        ledger.observe(&expect()),
        FundingObservation::Mismatch(MismatchReason::Recipient)
    );
}

#[test]
fn ledger_result_flows_through_require_funded() {
    // End to end: a deep, matching ledger entry passes the full gate; a shallow one is rejected.
    let mut ledger = LedgerVerifier::new();
    ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 2));
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), 3),
        Err(FundingRejected::TooShallow { have: 2, need: 3 })
    );
    ledger.fund(on_chain(vec![0xA1; 20], [0x11; HASH_LEN], 3));
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), 3),
        Ok(3)
    );
}

// ---- G3 / #74: per-chain confirmation policy + reorg re-verification ----

#[test]
fn confirmation_policy_testnet_defaults_are_per_chain() {
    let p = ConfirmationPolicy::testnet_defaults();
    // Per-chain depths, increasing with reorg risk / finality uncertainty (see the ADR).
    assert_eq!(p.required(Asset::Nim), 2);
    assert_eq!(p.required(Asset::Btc), 3);
    assert_eq!(p.required(Asset::Usdc), 5);
    // Default == the testnet defaults, so an un-configured node is safe-by-default.
    assert_eq!(ConfirmationPolicy::default(), p);
}

#[test]
fn confirmation_policy_required_for_leg_resolves_counterparty_chain() {
    let p = ConfirmationPolicy::testnet_defaults();
    // The NIM leg always uses the NIM depth, whatever the counterparty chain is.
    assert_eq!(p.required_for_leg(SwapLegId::Nim, Asset::Btc), 2);
    assert_eq!(p.required_for_leg(SwapLegId::Nim, Asset::Usdc), 2);
    // The counterparty leg uses *that* chain's depth — BTC and USDC differ.
    assert_eq!(p.required_for_leg(SwapLegId::Counterparty, Asset::Btc), 3);
    assert_eq!(p.required_for_leg(SwapLegId::Counterparty, Asset::Usdc), 5);
}

#[test]
fn confirmation_policy_uniform_and_builder_override() {
    assert_eq!(ConfirmationPolicy::uniform(4).required(Asset::Btc), 4);
    let p = ConfirmationPolicy::testnet_defaults().with_btc(6);
    assert_eq!(p.required(Asset::Btc), 6);
    assert_eq!(p.required(Asset::Nim), 2); // other chains unchanged
}

#[test]
fn shallow_then_deep_advances_only_when_buried_at_policy_depth() {
    // Depth < policy never advances; the same HTLC, once buried to the policy depth, passes.
    let need = ConfirmationPolicy::testnet_defaults().required(Asset::Nim);
    assert!(need >= 2, "test assumes NIM depth > 1");
    let htlc = |confs| OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock: [0x11; HASH_LEN],
        recipient: vec![0xA1; 20],
        amount: 100_000,
        timeout: 10_000,
        confirmations: confs,
    };
    let mut ledger = LedgerVerifier::new();
    ledger.fund(htlc(need - 1)); // one block short of the policy depth
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), need),
        Err(FundingRejected::TooShallow {
            have: need - 1,
            need
        })
    );
    ledger.fund(htlc(need)); // buried to the policy depth
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), need),
        Ok(need)
    );
}

#[test]
fn a_reorg_below_policy_depth_refuses_again() {
    // Re-verify on reorg (#74/G3): an HTLC buried deep enough to pass can be re-orged shallower;
    // the same gate must refuse it AGAIN — no funded transition may rest on a leg that reorged
    // below its policy depth.
    let need = ConfirmationPolicy::testnet_defaults().required(Asset::Nim);
    assert!(need >= 2, "test assumes NIM depth > 1");
    let mut ledger = LedgerVerifier::new();
    ledger.fund(OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock: [0x11; HASH_LEN],
        recipient: vec![0xA1; 20],
        amount: 100_000,
        timeout: 10_000,
        confirmations: need + 4, // comfortably buried — passes now
    });
    assert!(require_funded(&ledger.observe(&expect()), &expect(), need).is_ok());

    // A reorg re-buries the funding tx below the policy depth → the gate refuses it again.
    ledger.reorg_to(need - 1);
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), need),
        Err(FundingRejected::TooShallow {
            have: need - 1,
            need
        })
    );
}

#[test]
fn a_reorg_that_orphans_the_funding_tx_reads_as_absent() {
    // A deep reorg can orphan the funding tx entirely (depth → gone). The gate then sees nothing
    // on-chain, i.e. NotFundedYet — the honest "wait / eventually refund" path, never an advance.
    let mut ledger = LedgerVerifier::new();
    ledger.fund(OnChainHtlc {
        leg: SwapLegId::Nim,
        hashlock: [0x11; HASH_LEN],
        recipient: vec![0xA1; 20],
        amount: 100_000,
        timeout: 10_000,
        confirmations: 8,
    });
    assert!(matches!(
        ledger.observe(&expect()),
        FundingObservation::Found { .. }
    ));
    ledger.orphan_all();
    assert_eq!(ledger.observe(&expect()), FundingObservation::Absent);
    assert_eq!(
        require_funded(&ledger.observe(&expect()), &expect(), 1),
        Err(FundingRejected::NotFundedYet)
    );
}

#[test]
fn claim_observation_defaults_fail_closed_and_the_sim_verifiers_answer_by_role() {
    // Run-4 fix: the claim watch's fail-closed geometry. The TRAIT default is Unavailable — a
    // verifier that never implemented `observe_claim` can never settle a responder (SimVerifier
    // inherits it). The accept-all sim chain answers maximally buried, so the deterministic mesh
    // suites settle message-synchronously (C1 keeps it off the money path forever). The ledger
    // reference is an actual claim registry: NotFound until included, then the seeded depth.
    let sim = SimVerifier::healthy(100_000, 10_000, 10);
    assert_eq!(
        sim.observe_claim(SwapLegId::Nim, &[0x5A; HASH_LEN]),
        ClaimObservation::Unavailable
    );
    assert_eq!(
        AcceptAllVerifier.observe_claim(SwapLegId::Nim, &[0x5A; HASH_LEN]),
        ClaimObservation::Included {
            confirmations: u32::MAX
        }
    );

    let mut ledger = LedgerVerifier::new();
    assert_eq!(
        ledger.observe_claim(SwapLegId::Nim, &[0x5A; HASH_LEN]),
        ClaimObservation::NotFound
    );
    ledger.include_claim([0x5A; HASH_LEN], 1);
    assert_eq!(
        ledger.observe_claim(SwapLegId::Nim, &[0x5A; HASH_LEN]),
        ClaimObservation::Included { confirmations: 1 }
    );
    ledger.include_claim([0x5A; HASH_LEN], 7); // buried deeper in place, no duplicate entry
    assert_eq!(
        ledger.observe_claim(SwapLegId::Nim, &[0x5A; HASH_LEN]),
        ClaimObservation::Included { confirmations: 7 }
    );
    // A different tx id is a different claim.
    assert_eq!(
        ledger.observe_claim(SwapLegId::Nim, &[0x66; HASH_LEN]),
        ClaimObservation::NotFound
    );
}
