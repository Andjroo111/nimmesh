//! # swap_claim — the responder's claim ladder: broadcast → chain-confirm → settle (run-4 fix)
//!
//! The 2026-07-19 testnet soak (run 4, v0.88.4) proved a one-sided money loss: the initiator's
//! `withdraw(S)` mined on Amoy (S public, USDC gone), the responder's `claim_nim` failed on a
//! single transient `rpc http 429`, and the driver **settled anyway** — `let _ = sign_claim(…)`
//! then an unconditional `settle()` in `drive_phase_action`'s `(Responder, Revealed)` arm. One
//! tick later the GC reaped the "Settled" coordinator; nothing ever retried the still-claimable
//! 5-NIM HTLC, and at the timelock the initiator refunds it. A rate-limited shared IP at exactly
//! the claim moment = a permanent loss — a REAL mainnet risk (the phones ride the same path).
//!
//! This module is the fix's home, M3-symmetric with the initiator's gated reveal:
//!
//! 1. **Broadcast is not settlement.** The `(Responder, Revealed)` drive now only RECORDS a
//!    successful claim broadcast ([`SwapCoordinator::note_claim_broadcast`]); a failed one leaves
//!    the phase at `Revealed` — funds-safe, never reapable (`Revealed` has funds locked).
//! 2. **Retry on the tick cadence.** Both heartbeats ([`crate::swap_node::gc_tick`] slow,
//!    [`crate::swap_fast_tick`] fast) run [`drive_responder_claims`]: re-attempt unbroadcast
//!    claims, then consult the chain for broadcast ones.
//! 3. **Settle only on chain truth.** [`SwapSession::confirm_claim`] asks the session's
//!    [`FundingVerifier::observe_claim`] and settles ONLY on `Included` at the NIM leg's
//!    confirmation depth. `NotFound` re-arms the broadcast; `Unavailable` (a 429!) changes
//!    nothing. The sim `AcceptAllVerifier` answers maximally-buried, so the deterministic mesh
//!    suites settle on the same message-driven beat as before.
//! 4. **Foreclose honestly.** Past `T_A` unconfirmed, the session tick flips the swap to the
//!    honest terminal [`SwapPhase::Lost`](crate::swap::SwapPhase) — never a silent `Settled`,
//!    never a fictitious `Refunded` (see `Swap::forfeit_claim`).

use crate::engine::{WorkerCtx, WorkerState};
use crate::swap::{SwapPhase, SwapRole};
use crate::swap_coordinator::SwapCoordinator;
use crate::swap_funding_verify::ClaimObservation;
use crate::swap_session::{telemetry_now_ms, SwapSession, SwapVerifyNote};
use crate::swap_wire::{SwapLegId, SWAP_ID_LEN};

/// Whether `c` is a responder holding claim work: at `Revealed` with either no broadcast claim
/// yet (attempt one) or a broadcast claim awaiting its confirmation consult.
fn has_claim(c: &SwapCoordinator) -> bool {
    c.role() == SwapRole::Responder && c.phase() == SwapPhase::Revealed
}

impl SwapSession {
    /// Whether ANY in-flight swap has responder-claim work — the fast-tick work-exists gate.
    pub(crate) fn has_claim_work(&self) -> bool {
        self.coordinators.values().any(has_claim)
    }

    /// The responder swaps at `Revealed` with NO broadcast claim recorded — the tick re-drives
    /// `drive_phase_action` for these so a failed claim broadcast retries until it lands or the
    /// timelock forecloses.
    pub(crate) fn claims_awaiting_broadcast(&self) -> Vec<[u8; SWAP_ID_LEN]> {
        self.coordinators
            .iter()
            .filter(|(_, c)| has_claim(c) && c.claim_broadcast().is_none())
            .map(|(id, _)| *id)
            .collect()
    }

    /// One claim-confirmation consult for `swap_id` (run-4 fix): ask the verifier about our OWN
    /// broadcast NIM-claim tx and act on the three-valued answer — `Included` at the NIM leg's
    /// depth → `settle()` (the ONLY path to a responder `Settled`); `Included` shallower → keep
    /// waiting; `NotFound` → clear the broadcast so the tick re-claims; `Unavailable` → change
    /// nothing (fail-closed — a 429 must never move state). Records a verify note either way so
    /// the operator SEES the claim's progress. Returns whether the swap settled.
    pub(crate) fn confirm_claim(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> bool {
        let Some(tx_id) = self
            .coordinators
            .get(swap_id)
            .filter(|c| has_claim(c))
            .and_then(|c| c.claim_broadcast())
        else {
            return false;
        };
        let need = self
            .confirm_policy
            .required_for_leg(SwapLegId::Nim, self.counterparty_chain);
        let obs = self.verifier.observe_claim(SwapLegId::Nim, &tx_id);
        let (settled, text) = match obs {
            ClaimObservation::Included { confirmations } if confirmations >= need => {
                (true, "claim confirmed — settled".to_string())
            }
            ClaimObservation::Included { confirmations } => {
                (false, format!("claim too shallow {confirmations}/{need}"))
            }
            ClaimObservation::NotFound => (false, "claim not on-chain — re-claiming".to_string()),
            ClaimObservation::Unavailable => (false, "claim status unavailable".to_string()),
        };
        if let Some(c) = self.coordinators.get_mut(swap_id) {
            if settled {
                let _ = c.settle();
            } else if obs == ClaimObservation::NotFound {
                c.clear_claim_broadcast(); // the tick re-attempts the claim next beat
            }
        }
        let note = SwapVerifyNote {
            text,
            attempts: self
                .verify_notes
                .get(swap_id)
                .map_or(0, |n| n.attempts)
                .saturating_add(1),
            at_ms: telemetry_now_ms(),
        };
        self.verify_notes.insert(*swap_id, note);
        settled
    }

    /// Run [`confirm_claim`](Self::confirm_claim) for every broadcast-but-unconfirmed responder
    /// claim; returns the swap_ids that SETTLED this pass.
    pub(crate) fn confirm_pending_claims(&mut self) -> Vec<[u8; SWAP_ID_LEN]> {
        let pending: Vec<[u8; SWAP_ID_LEN]> = self
            .coordinators
            .iter()
            .filter(|(_, c)| has_claim(c) && c.claim_broadcast().is_some())
            .map(|(id, _)| *id)
            .collect();
        pending
            .into_iter()
            .filter(|id| self.confirm_claim(id))
            .collect()
    }
}

/// The responder-claim step both tick cadences share: FIRST confirm broadcast claims against the
/// chain (a claim broadcast last beat settles the moment it is buried), THEN (re)attempt the
/// claim broadcast for every `Revealed` responder still lacking one — via the same
/// [`drive_phase_action`](crate::swap_node::drive_phase_action) the message path uses, so the
/// phase remains the single idempotency guard. No mesh envelopes result (the claim lives
/// on-chain); the callers re-sync the phase mirror afterwards.
pub(crate) fn drive_responder_claims(ctx: &WorkerCtx, st: &mut WorkerState, head: u64) {
    let Some(session) = st.swap.as_mut() else {
        return;
    };
    if !session.has_claim_work() {
        return; // the common case: no Revealed responder — zero allocs, zero consults
    }
    let _settled = session.confirm_pending_claims();
    let retry = session.claims_awaiting_broadcast();
    for swap_id in retry {
        for (mt, payload) in crate::swap_node::drive_phase_action(st, swap_id, head) {
            crate::swap_node::flood_swap_reply(ctx, mt, payload, st);
        }
    }
}

/// The `(Responder, Revealed)` drive arm (run-4 fix), called from
/// [`drive_phase_action`](crate::swap_node::drive_phase_action): attempt the claim broadcast if
/// none is recorded (a signer failure leaves the phase at `Revealed` — the ticks retry), then run
/// ONE immediate confirmation consult so a chain that already shows the claim buried (the sim's
/// accept-all; a fast finality) settles without waiting a beat.
pub(crate) fn attempt_responder_claim(
    st: &mut WorkerState,
    swap_id: [u8; SWAP_ID_LEN],
    secret: Option<[u8; 32]>,
    sctx: &crate::swap_coordinator::SwapContext,
) {
    let already = st
        .swap
        .as_mut()
        .and_then(|s| s.coordinator(&swap_id))
        .and_then(|c| c.claim_broadcast())
        .is_some();
    if !already {
        let built = secret.and_then(|s| {
            st.signer
                .as_ref()
                .and_then(|signer| signer.build_claim(sctx, s))
        });
        if let Some((_wire, tx_id)) = built {
            if let Some(c) = st.swap.as_mut().and_then(|s| s.coordinator(&swap_id)) {
                c.note_claim_broadcast(tx_id);
            }
        }
        // On None: no broadcast happened (build/broadcast failed — run 4's 429). Stay at
        // `Revealed`; the gc/fast ticks re-enter this arm until it lands or T_A forecloses.
    }
    if let Some(session) = st.swap.as_mut() {
        let _ = session.confirm_claim(&swap_id);
    }
}

#[cfg(test)]
#[path = "swap_claim_tests.rs"]
mod tests;
