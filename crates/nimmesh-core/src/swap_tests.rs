//! # swap_tests — the `swap` state-machine suite (`#[path]` child of `swap.rs`, split out for
//! the 800-line guard). `super` is `crate::swap`, so every item stays reachable as before.

use super::*;

/// A safe ladder at head 0: T_A and T_B both far ahead, big margin.
fn safe_terms() -> SwapTerms {
    SwapTerms {
        nim_timeout: 10_000,
        counterparty_timeout: 5_000, // margin 5000 >= 3600; window 5000 >= 1800
    }
}

#[test]
fn ladder_safe_when_well_laddered() {
    assert_eq!(
        assess_ladder(&safe_terms(), 0, &LadderParams::default()),
        LadderVerdict::Safe
    );
}

#[test]
fn ladder_rejects_inversion_thin_margin_and_short_window() {
    let p = LadderParams::default();
    // Inverted: T_A <= T_B.
    assert_eq!(
        assess_ladder(
            &SwapTerms {
                nim_timeout: 5_000,
                counterparty_timeout: 5_000
            },
            0,
            &p
        ),
        LadderVerdict::Inverted
    );
    // Thin margin: T_A - T_B = 1000 < 3600.
    assert!(matches!(
        assess_ladder(
            &SwapTerms {
                nim_timeout: 6_000,
                counterparty_timeout: 5_000
            },
            0,
            &p
        ),
        LadderVerdict::MarginTooThin { have: 1000, .. }
    ));
    // Short window: T_B - head = 1000 < 1800 (head crept up to 4000).
    assert!(matches!(
        assess_ladder(&safe_terms(), 4_000, &p),
        LadderVerdict::WindowTooShort { have: 1000, .. }
    ));
}

#[test]
fn accept_refuses_unsafe_ladder() {
    let mut s = Swap::new(
        SwapRole::Initiator,
        SwapTerms {
            nim_timeout: 6_000,
            counterparty_timeout: 5_000, // thin margin
        },
    );
    assert!(matches!(
        s.accept(0, &LadderParams::default()),
        Err(SwapError::UnsafeLadder(LadderVerdict::MarginTooThin { .. }))
    ));
    // Phase unchanged — nothing committed.
    assert_eq!(s.phase, SwapPhase::Proposed);
}

#[test]
fn initiator_happy_path_to_settled() {
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Initiator, safe_terms());
    s.accept(0, &p).unwrap();
    assert_eq!(
        s.fund(0, &p).unwrap(),
        SwapAction::FundLeg {
            leg: SwapLegId::Nim,
            timeout: 10_000
        }
    );
    s.observe_counterparty_funded().unwrap();
    assert_eq!(
        s.reveal_and_claim(0, &p).unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    s.observe_settled().unwrap();
    assert_eq!(s.phase, SwapPhase::Settled);
}

#[test]
fn reveal_refuses_when_the_counterparty_timeout_is_too_close() {
    // G4 / #75: the initiator claims the counterparty leg (T_B = 5000). If the head has crept to
    // within the claim window (1800) of T_B, revealing burns the secret with too little time to
    // claim safely → refuse, and keep the secret secret (phase unchanged, refund still the exit).
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Initiator, safe_terms());
    s.accept(0, &p).unwrap();
    s.fund(0, &p).unwrap();
    s.observe_counterparty_funded().unwrap(); // BothFunded, secret still held
    assert_eq!(s.reveal_deadline_margin(4_000), 1_000); // 5000 - 4000, below the 1800 window
    assert!(matches!(
        s.reveal_and_claim(4_000, &p),
        Err(SwapError::RevealTooLate(RevealVerdict::DeadlineTooClose {
            have: 1_000,
            need: 1_800
        }))
    ));
    assert_eq!(s.phase, SwapPhase::BothFunded); // never revealed

    // Earlier, with room to spare, the same reveal is allowed.
    assert_eq!(
        s.reveal_and_claim(0, &p).unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Counterparty
        }
    );
    assert_eq!(s.phase, SwapPhase::Revealed);
}

#[test]
fn assess_reveal_deadline_is_safe_with_room_and_close_at_the_edge() {
    let p = LadderParams::default();
    let terms = safe_terms(); // T_B = 5000, window need = 1800
    assert_eq!(assess_reveal_deadline(&terms, 0, &p), RevealVerdict::Safe);
    // Exactly at the boundary: window == need is still safe (need is a floor).
    assert_eq!(
        assess_reveal_deadline(&terms, 3_200, &p),
        RevealVerdict::Safe
    );
    // One block past the boundary → too close.
    assert_eq!(
        assess_reveal_deadline(&terms, 3_201, &p),
        RevealVerdict::DeadlineTooClose {
            have: 1_799,
            need: 1_800
        }
    );
    // Past T_B entirely → window saturates to 0.
    assert_eq!(
        assess_reveal_deadline(&terms, 9_999, &p),
        RevealVerdict::DeadlineTooClose {
            have: 0,
            need: 1_800
        }
    );
}

#[test]
fn responder_must_observe_initiator_funding_before_funding() {
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Responder, safe_terms());
    s.accept(0, &p).unwrap();
    // Responder cannot fund straight from Accepted — must see the initiator lock first.
    assert!(matches!(
        s.fund(0, &p),
        Err(SwapError::IllegalTransition { action: "fund", .. })
    ));
    s.observe_initiator_funded().unwrap();
    assert_eq!(
        s.fund(0, &p).unwrap(),
        SwapAction::FundLeg {
            leg: SwapLegId::Counterparty,
            timeout: 5_000
        }
    );
    // Responder claims the NIM leg once the secret is out.
    s.observe_counterparty_funded().unwrap();
    assert_eq!(
        s.observe_secret().unwrap(),
        SwapAction::ClaimLeg {
            leg: SwapLegId::Nim
        }
    );
    s.observe_settled().unwrap();
    assert_eq!(s.phase, SwapPhase::Settled);
}

#[test]
fn fund_rechecks_safety_at_fund_time() {
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Initiator, safe_terms());
    s.accept(0, &p).unwrap(); // safe at head 0
                              // Head crept forward so the window is now too short to fund safely.
    assert!(matches!(
        s.fund(4_000, &p),
        Err(SwapError::UnsafeLadder(
            LadderVerdict::WindowTooShort { .. }
        ))
    ));
    assert_eq!(s.phase, SwapPhase::Accepted); // not funded
}

#[test]
fn refund_only_after_timeout_and_only_when_funded() {
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Initiator, safe_terms());
    s.accept(0, &p).unwrap();
    // Can't refund before funding.
    assert!(matches!(
        s.refund_after_timeout(99_999),
        Err(SwapError::IllegalTransition {
            action: "refund",
            ..
        })
    ));
    s.fund(0, &p).unwrap();
    // Can't refund before the timeout height (own leg = nim_timeout = 10_000).
    assert!(matches!(
        s.refund_after_timeout(9_999),
        Err(SwapError::TimeoutNotReached {
            timeout: 10_000,
            ..
        })
    ));
    // After the timeout, refund is allowed.
    assert_eq!(
        s.refund_after_timeout(10_001).unwrap(),
        SwapAction::RefundLeg {
            leg: SwapLegId::Nim
        }
    );
    assert_eq!(s.phase, SwapPhase::Refunded);
}

#[test]
fn abort_is_legal_only_before_funding() {
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Initiator, safe_terms());
    s.accept(0, &p).unwrap();
    let mut pre = s.clone();
    pre.abort().unwrap();
    assert_eq!(pre.phase, SwapPhase::Aborted);
    // Once funded, abort is illegal — only claim or timeout-refund resolves it.
    s.fund(0, &p).unwrap();
    assert!(matches!(
        s.abort(),
        Err(SwapError::IllegalTransition {
            action: "abort",
            ..
        })
    ));
}

#[test]
fn every_funded_phase_keeps_a_refund_exit() {
    // The atomicity guarantee: in any phase where funds are locked, refund_after_timeout is a
    // legal action (past the timeout). This is what makes "worst case = refund, never theft".
    for phase in [
        SwapPhase::SelfFunded,
        SwapPhase::BothFunded,
        SwapPhase::Revealed,
    ] {
        assert!(phase.has_funds_locked());
        let mut s = Swap::new(SwapRole::Initiator, safe_terms());
        s.phase = phase;
        assert!(
            s.refund_after_timeout(safe_terms().nim_timeout + 1).is_ok(),
            "phase {phase:?} must allow a timeout refund"
        );
    }
}

#[test]
fn a_revealed_responder_cannot_fake_refund_and_forfeits_only_past_t_a() {
    // Run-4 fix: once a responder is Revealed, its own (counterparty) leg is already claimed with
    // the public S — `refund_after_timeout` there would stamp a real loss "Refunded". Its honest
    // exits are settle (claim confirmed) or, past T_A, the Lost terminal via `forfeit_claim`.
    let p = LadderParams::default();
    let mut s = Swap::new(SwapRole::Responder, safe_terms());
    s.accept(0, &p).unwrap();
    s.observe_initiator_funded().unwrap();
    s.fund(0, &p).unwrap();
    s.observe_counterparty_funded().unwrap();
    s.observe_secret().unwrap();
    assert_eq!(s.phase, SwapPhase::Revealed);

    // No fictitious refund — not even far past its own T_B (5_000).
    assert!(matches!(
        s.refund_after_timeout(99_999),
        Err(SwapError::IllegalTransition {
            action: "refund",
            ..
        })
    ));
    // Forfeiture is gated on T_A (10_000), the timeout of the leg the responder claims.
    assert!(matches!(
        s.forfeit_claim(10_000),
        Err(SwapError::TimeoutNotReached {
            timeout: 10_000,
            ..
        })
    ));
    assert_eq!(s.phase, SwapPhase::Revealed);
    s.forfeit_claim(10_001).unwrap();
    assert_eq!(s.phase, SwapPhase::Lost);
    assert!(SwapPhase::Lost.is_terminal());
    assert!(!SwapPhase::Lost.has_funds_locked());
}

#[test]
fn forfeit_claim_is_illegal_for_initiators_and_outside_revealed() {
    let p = LadderParams::default();
    // An initiator in Revealed keeps its real refund exit and can never "forfeit".
    let mut init = Swap::new(SwapRole::Initiator, safe_terms());
    init.accept(0, &p).unwrap();
    init.fund(0, &p).unwrap();
    init.observe_counterparty_funded().unwrap();
    init.reveal_and_claim(0, &p).unwrap();
    assert!(matches!(
        init.forfeit_claim(99_999),
        Err(SwapError::IllegalTransition { .. })
    ));
    assert!(init.refund_after_timeout(10_001).is_ok()); // the initiator refund is untouched

    // A responder before Revealed forfeits nothing — its funded phases refund as ever.
    let mut resp = Swap::new(SwapRole::Responder, safe_terms());
    resp.accept(0, &p).unwrap();
    resp.observe_initiator_funded().unwrap();
    resp.fund(0, &p).unwrap();
    assert!(matches!(
        resp.forfeit_claim(99_999),
        Err(SwapError::IllegalTransition { .. })
    ));
    assert!(resp.refund_after_timeout(5_001).is_ok()); // own leg T_B refund still real
}
