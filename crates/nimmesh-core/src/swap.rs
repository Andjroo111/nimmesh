//! # swap — the offline atomic-swap state machine + the `Δ_safe` safety gate (mesh swap F3)
//!
//! The clock-free, height-anchored lifecycle of a single cross-chain HTLC atomic swap
//! (`docs/swap/SWAP.md`). This module owns the **rules**, not the bytes or the radio: it decides
//! *when it is safe to fund*, *what to fund/claim*, and *which transitions are legal*, given only
//! the agreed [`SwapTerms`] and the current mesh head height (from the G9 head-beacon — the mesh's
//! only clock). The Nimiq HTLC tx bytes live in [`crate::nimiq::htlc`]; the wire messages in
//! [`crate::swap_wire`]; the mesh wiring is F4. No keys, no seed, no preimage-before-reveal here.
//!
//! ## The safety invariant (the whole point)
//!
//! An HTLC swap is only *atomic* (worst case a refund, never a theft) if the timelocks ladder:
//! the initiator — who reveals the secret to claim — must have **more** time than the responder.
//! Over a high-latency, partition-prone BLE mesh, the margin must dominate the worst-case
//! propagation window (store-and-forward ~15 min ≈ 900 blocks at ~1 block/s). [`assess_ladder`]
//! encodes this, and the state machine **refuses to fund** when the ladder is unsafe — an honest
//! "can't swap safely here" beats a silent loss (`docs/swap/FEASIBILITY.md`).
//!
//! Standard convention: the **initiator gives the NIM leg with the *longer* timeout `T_A`** and
//! claims the counterparty leg; the **responder funds the counterparty leg with the *shorter*
//! timeout `T_B`** (`T_B < T_A`) and claims the NIM leg with the revealed secret.

use crate::swap_wire::SwapLegId;

/// One Nimiq block is ≈ 1 second, so block counts double as a coarse wall-clock budget.
/// The mesh store-and-forward catch-up window is ~15 min ≈ 900 blocks; the safety margin must
/// dominate it. These are conservative defaults — see [`LadderParams`].
pub const BLOCKS_PER_SECOND: u64 = 1;
/// Default required margin between the two timelocks (`T_A − T_B`): ~1 hour of blocks. Must
/// exceed the worst-case (mesh propagation + counterparty claim confirmation).
pub const DEFAULT_DELTA_SAFE_BLOCKS: u64 = 3600;
/// Default minimum room the *shorter* timeout needs beyond the current head before a swap may be
/// funded (time to fund + observe + claim the counterparty leg): ~30 min of blocks.
pub const DEFAULT_MIN_CLAIM_WINDOW_BLOCKS: u64 = 1800;

/// Which side of the swap this node is playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapRole {
    /// Gives the NIM leg (longer timeout `T_A`), **knows the secret**, claims the counterparty
    /// leg by revealing it. Funds first.
    Initiator,
    /// Gives the counterparty leg (shorter timeout `T_B`), claims the NIM leg with the revealed
    /// secret. Funds only **after** observing the initiator's funding on-chain.
    Responder,
}

/// The agreed, public terms of a swap. Amounts are in each chain's base unit (NIM = luna).
///
/// **Timeouts are a TIME quantity, not a block height** (corrected after the live testnet proof:
/// the NIM HTLC timeout is a Unix-MILLISECOND timestamp compared to the block time — see
/// `nimiq::htlc`). Both legs' timeouts + `Δ_safe` must be expressed in the **same comparable unit**
/// (Unix time) for the ladder check to be meaningful; the head-beacon's block *time* is the clock.
/// `assess_ladder` is unit-agnostic — feed it consistent units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapTerms {
    /// `T_A` — the NIM-leg HTLC timeout (the **longer** one).
    pub nim_timeout: u64,
    /// `T_B` — the counterparty-leg timeout (the **shorter** one; `T_B < T_A`).
    pub counterparty_timeout: u64,
}

/// Tunable thresholds for the timelock-ladder safety check. Defaults are conservative for a mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderParams {
    /// Minimum required `T_A − T_B` in blocks.
    pub delta_safe_blocks: u64,
    /// Minimum required `T_B − head` in blocks before funding is allowed.
    pub min_claim_window_blocks: u64,
}

impl Default for LadderParams {
    fn default() -> Self {
        LadderParams {
            delta_safe_blocks: DEFAULT_DELTA_SAFE_BLOCKS,
            min_claim_window_blocks: DEFAULT_MIN_CLAIM_WINDOW_BLOCKS,
        }
    }
}

/// The verdict of the timelock-ladder safety check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderVerdict {
    /// The ladder is safe to fund at this head height.
    Safe,
    /// `T_A ≤ T_B` — the ladder is inverted; the initiator could steal both legs. Never fund.
    Inverted,
    /// `T_A − T_B` is positive but smaller than `delta_safe_blocks`.
    MarginTooThin {
        /// The actual `T_A − T_B`.
        have: u64,
        /// The required `delta_safe_blocks`.
        need: u64,
    },
    /// The shorter timeout is too close to (or behind) the current head to fund + claim safely.
    WindowTooShort {
        /// Blocks remaining until `T_B` (`0` if already past).
        have: u64,
        /// The required `min_claim_window_blocks`.
        need: u64,
    },
}

impl LadderVerdict {
    /// Whether funding is permitted under this verdict.
    pub const fn is_safe(self) -> bool {
        matches!(self, LadderVerdict::Safe)
    }
}

/// The headline safety gate: is it safe to fund this swap at `current_head`?
///
/// Checks, in order: the ladder is not inverted (`T_A > T_B`); the margin `T_A − T_B` meets
/// `delta_safe_blocks`; and the shorter timeout `T_B` is far enough ahead of the head
/// (`T_B − head ≥ min_claim_window_blocks`) to fund, observe, and claim before it expires. Pure
/// and clock-free — `current_head` comes from the head-beacon, never a wall clock.
pub fn assess_ladder(terms: &SwapTerms, current_head: u64, params: &LadderParams) -> LadderVerdict {
    if terms.nim_timeout <= terms.counterparty_timeout {
        return LadderVerdict::Inverted;
    }
    let margin = terms.nim_timeout - terms.counterparty_timeout;
    if margin < params.delta_safe_blocks {
        return LadderVerdict::MarginTooThin {
            have: margin,
            need: params.delta_safe_blocks,
        };
    }
    let window = terms.counterparty_timeout.saturating_sub(current_head);
    if window < params.min_claim_window_blocks {
        return LadderVerdict::WindowTooShort {
            have: window,
            need: params.min_claim_window_blocks,
        };
    }
    LadderVerdict::Safe
}

/// The lifecycle phase of a swap, from one node's perspective. Every phase in which this node has
/// funds locked has a refund exit (`refund_after_timeout`), so the worst case is always a refund.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapPhase {
    /// Terms are on the table; nothing is funded.
    Proposed,
    /// Both sides agreed; the ladder was checked safe at accept time.
    Accepted,
    /// (Responder only) the initiator's funding was observed on-chain; safe for the responder
    /// to fund its leg.
    InitiatorFunded,
    /// This node has funded its leg; waiting on / has confirmed both legs.
    SelfFunded,
    /// Both legs are funded on-chain.
    BothFunded,
    /// The secret is out — the initiator claimed the counterparty leg (revealing it), or the
    /// responder observed it. Both can now claim.
    Revealed,
    /// This node's claim has settled on-chain. Terminal, success.
    Settled,
    /// Cancelled before this node funded. Terminal, no funds at risk.
    Aborted,
    /// This node's locked funds were refunded via the timeout path. Terminal, no loss.
    Refunded,
}

impl SwapPhase {
    /// Whether this node currently has funds locked in an HTLC (so a refund path must stay open).
    pub const fn has_funds_locked(self) -> bool {
        matches!(
            self,
            SwapPhase::SelfFunded | SwapPhase::BothFunded | SwapPhase::Revealed
        )
    }

    /// Whether this is a terminal phase (no further transitions).
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            SwapPhase::Settled | SwapPhase::Aborted | SwapPhase::Refunded
        )
    }
}

/// An illegal or unsafe state-machine action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapError {
    /// The action is not legal from the current phase for this role.
    IllegalTransition {
        /// The phase the swap was in.
        from: SwapPhase,
        /// A short name for the attempted action.
        action: &'static str,
    },
    /// Funding was refused because the timelock ladder is not safe at the current head.
    UnsafeLadder(LadderVerdict),
    /// A refund was attempted before the relevant timeout height elapsed.
    TimeoutNotReached {
        /// The timeout height that must be passed first.
        timeout: u64,
        /// The current head height.
        head: u64,
    },
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapError::IllegalTransition { from, action } => {
                write!(f, "illegal swap action {action:?} from phase {from:?}")
            }
            SwapError::UnsafeLadder(v) => {
                write!(f, "refusing to fund: unsafe timelock ladder ({v:?})")
            }
            SwapError::TimeoutNotReached { timeout, head } => {
                write!(f, "refund not yet allowed: timeout {timeout} > head {head}")
            }
        }
    }
}

impl std::error::Error for SwapError {}

/// What the caller should do on chain after a successful transition (F4 turns this into HTLC txs
/// via [`crate::nimiq::htlc`] / the counterparty leg).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAction {
    /// Fund the given leg's HTLC (lock funds to the hashlock with the given timeout).
    FundLeg {
        /// Which chain to fund on.
        leg: SwapLegId,
        /// The HTLC timeout height for that leg.
        timeout: u64,
    },
    /// Claim the given leg's HTLC by revealing the secret (initiator) or using the revealed
    /// secret (responder).
    ClaimLeg {
        /// Which chain to claim on.
        leg: SwapLegId,
    },
    /// Refund this node's own funded HTLC via the timeout path.
    RefundLeg {
        /// Which chain to refund on.
        leg: SwapLegId,
    },
    /// No on-chain action — a bookkeeping transition only.
    None,
}

/// A single swap, from this node's perspective. Clock-free: every method that depends on time
/// takes the current head height explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swap {
    /// Which side this node plays.
    pub role: SwapRole,
    /// The agreed timelocks.
    pub terms: SwapTerms,
    /// The current lifecycle phase.
    pub phase: SwapPhase,
}

impl Swap {
    /// Start a swap in [`SwapPhase::Proposed`].
    pub fn new(role: SwapRole, terms: SwapTerms) -> Self {
        Swap {
            role,
            terms,
            phase: SwapPhase::Proposed,
        }
    }

    /// The leg this node funds (initiator → NIM with `T_A`; responder → counterparty with `T_B`).
    pub const fn own_leg(&self) -> SwapLegId {
        match self.role {
            SwapRole::Initiator => SwapLegId::Nim,
            SwapRole::Responder => SwapLegId::Counterparty,
        }
    }

    /// The leg this node claims (the counterparty's funded leg).
    pub const fn claim_leg(&self) -> SwapLegId {
        match self.role {
            SwapRole::Initiator => SwapLegId::Counterparty,
            SwapRole::Responder => SwapLegId::Nim,
        }
    }

    /// The timeout height of this node's own funded leg.
    const fn own_timeout(&self) -> u64 {
        match self.role {
            SwapRole::Initiator => self.terms.nim_timeout,
            SwapRole::Responder => self.terms.counterparty_timeout,
        }
    }

    /// Move `Proposed → Accepted`, refusing if the ladder is unsafe at `current_head`. Both sides
    /// gate here so neither commits to a swap that can't be made safe offline.
    pub fn accept(&mut self, current_head: u64, params: &LadderParams) -> Result<(), SwapError> {
        if self.phase != SwapPhase::Proposed {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "accept",
            });
        }
        let verdict = assess_ladder(&self.terms, current_head, params);
        if !verdict.is_safe() {
            return Err(SwapError::UnsafeLadder(verdict));
        }
        self.phase = SwapPhase::Accepted;
        Ok(())
    }

    /// (Responder) record that the initiator's funding was observed on-chain.
    pub fn observe_initiator_funded(&mut self) -> Result<(), SwapError> {
        match (self.role, self.phase) {
            (SwapRole::Responder, SwapPhase::Accepted) => {
                self.phase = SwapPhase::InitiatorFunded;
                Ok(())
            }
            _ => Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "observe_initiator_funded",
            }),
        }
    }

    /// Fund this node's own leg — **re-checking the ladder is still safe** at `current_head`. The
    /// initiator funds from `Accepted`; the responder funds only from `InitiatorFunded` (i.e. only
    /// after seeing the initiator lock first), so the responder is never first-to-lock with
    /// nothing in return.
    pub fn fund(
        &mut self,
        current_head: u64,
        params: &LadderParams,
    ) -> Result<SwapAction, SwapError> {
        let legal = matches!(
            (self.role, self.phase),
            (SwapRole::Initiator, SwapPhase::Accepted)
                | (SwapRole::Responder, SwapPhase::InitiatorFunded)
        );
        if !legal {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "fund",
            });
        }
        let verdict = assess_ladder(&self.terms, current_head, params);
        if !verdict.is_safe() {
            return Err(SwapError::UnsafeLadder(verdict));
        }
        self.phase = SwapPhase::SelfFunded;
        Ok(SwapAction::FundLeg {
            leg: self.own_leg(),
            timeout: self.own_timeout(),
        })
    }

    /// Record that the counterparty's leg was observed funded on-chain → `BothFunded`.
    pub fn observe_counterparty_funded(&mut self) -> Result<(), SwapError> {
        if self.phase != SwapPhase::SelfFunded {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "observe_counterparty_funded",
            });
        }
        self.phase = SwapPhase::BothFunded;
        Ok(())
    }

    /// (Initiator) claim the counterparty leg, **revealing the secret** → `Revealed`. This is the
    /// only place the secret enters play, and it enters inside a broadcast-safe claim tx.
    pub fn reveal_and_claim(&mut self) -> Result<SwapAction, SwapError> {
        match (self.role, self.phase) {
            (SwapRole::Initiator, SwapPhase::BothFunded) => {
                self.phase = SwapPhase::Revealed;
                Ok(SwapAction::ClaimLeg {
                    leg: self.claim_leg(),
                })
            }
            _ => Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "reveal_and_claim",
            }),
        }
    }

    /// (Responder) the secret was observed (from the mesh reveal or the counterparty chain) →
    /// `Revealed`; the responder can now claim the NIM leg.
    pub fn observe_secret(&mut self) -> Result<SwapAction, SwapError> {
        match (self.role, self.phase) {
            (SwapRole::Responder, SwapPhase::BothFunded) => {
                self.phase = SwapPhase::Revealed;
                Ok(SwapAction::ClaimLeg {
                    leg: self.claim_leg(),
                })
            }
            _ => Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "observe_secret",
            }),
        }
    }

    /// Record that this node's claim settled on-chain → `Settled` (terminal success).
    pub fn observe_settled(&mut self) -> Result<(), SwapError> {
        if self.phase != SwapPhase::Revealed {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "observe_settled",
            });
        }
        self.phase = SwapPhase::Settled;
        Ok(())
    }

    /// Cancel a swap **before this node has funded** → `Aborted`. Illegal once funds are locked
    /// (then only claim or timeout-refund can resolve it — the timelock is the guarantee).
    pub fn abort(&mut self) -> Result<(), SwapError> {
        if self.phase.has_funds_locked() || self.phase.is_terminal() {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "abort",
            });
        }
        self.phase = SwapPhase::Aborted;
        Ok(())
    }

    /// Refund this node's own funded leg via the timeout path — only legal **after** its timeout
    /// height has elapsed (`current_head > own_timeout`). This is the always-available safety exit
    /// whenever a swap stalls: the worst case is getting your own money back.
    pub fn refund_after_timeout(&mut self, current_head: u64) -> Result<SwapAction, SwapError> {
        if !self.phase.has_funds_locked() {
            return Err(SwapError::IllegalTransition {
                from: self.phase,
                action: "refund",
            });
        }
        let timeout = self.own_timeout();
        if current_head <= timeout {
            return Err(SwapError::TimeoutNotReached {
                timeout,
                head: current_head,
            });
        }
        self.phase = SwapPhase::Refunded;
        Ok(SwapAction::RefundLeg {
            leg: self.own_leg(),
        })
    }
}

#[cfg(test)]
mod tests {
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
            s.reveal_and_claim().unwrap(),
            SwapAction::ClaimLeg {
                leg: SwapLegId::Counterparty
            }
        );
        s.observe_settled().unwrap();
        assert_eq!(s.phase, SwapPhase::Settled);
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
}
