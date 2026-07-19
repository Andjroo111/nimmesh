//! # swap_fast_tick — the ~3 s in-flight settlement heartbeat (the tick half of the sub-30 s goal)
//!
//! PR #227's tick-driven funding re-verification rides the worker's `BeaconTick` maintenance
//! heartbeat, which the shims drive at ~15 s (the iOS keepalive / mac-node timers). That cadence
//! alone puts several 15 s quanta in the settlement path: fund → (≤15 s) verify → next step →
//! (≤15 s) verify → reveal. This module adds a FAST poll the shims call every ~3 s while a swap
//! sheet is live: [`MeshNode::poll_swap_fast`] → `Job::SwapFastTick` → [`fast_tick`], which runs
//! **ONLY** the counterparty-funding re-verification
//! ([`SwapSession::fast_reverify`](crate::swap_session::SwapSession)) and, for each swap that
//! advanced, the same `drive_phase_action` money-path step the slow tick uses. Same fail-closed
//! gate, same depths, same code — just consulted more often.
//!
//! What the fast tick deliberately does **NOT** do (all stay on the slow `BeaconTick` cadence):
//! - **no retransmit drain** — `RETRANSMIT_TTL` (32) is counted in *slow* ticks; draining it here
//!   would gut the ~8 min retransmit budget to ~96 s (32 × 3 s) and flood the mesh 5× harder;
//! - **no beacon emit / gossip-sync / GC / match-window close** — none of them are settlement-
//!   latency-critical, and beacon flooding at 3 s would spam every peer in radio range.
//!
//! Two cost guards keep the 3 s cadence free when idle and polite when active:
//! 1. the session no-ops unless a proof-seen swap is actually awaiting counterparty funding
//!    (idle polls consult NOTHING — same rationale as the `funding_proof_seen` gate in #227);
//! 2. [`FastTickScheduler`] rate-limits the chain consult to one per [`FAST_VERIFY_TICK_MS`]
//!    on the worker's monotonic clock (the `BeaconScheduler` pattern — caller-driven,
//!    clock-free in tests), so a shim/webui that polls harder cannot hammer the shared-IP RPC.

use crate::engine::{WorkerCtx, WorkerState};
use crate::node::MeshNode;

/// The minimum spacing between fast in-flight chain consults (~3 s): fast enough that the
/// verify leg adds at most one small quantum to settlement, slow enough that a whole swap
/// costs a few dozen RPC reads. The shims' fast timers target the same period; this floor
/// is the core-side guarantee.
pub const FAST_VERIFY_TICK_MS: u64 = 3_000;

/// A clock-free rate-limiter for the fast re-verify (the [`crate::beacon::BeaconScheduler`]
/// pattern): [`due`](FastTickScheduler::due) returns `true` at most once per
/// [`FAST_VERIFY_TICK_MS`]; the caller passes a monotonic `now_ms`. The first poll always
/// fires. The budget is only consumed when it fires — the caller must check its work-exists
/// gate FIRST so idle polls never burn the slot.
#[derive(Debug, Default)]
pub struct FastTickScheduler {
    last_ms: Option<u64>,
}

impl FastTickScheduler {
    /// A fresh scheduler that is due on its first poll.
    pub fn new() -> Self {
        FastTickScheduler { last_ms: None }
    }

    /// Whether a fast re-verify is due at `now_ms`; records the firing time when `true`.
    pub fn due(&mut self, now_ms: u64) -> bool {
        match self.last_ms {
            Some(t) if now_ms.saturating_sub(t) < FAST_VERIFY_TICK_MS => false,
            _ => {
                self.last_ms = Some(now_ms);
                true
            }
        }
    }
}

/// The fast in-flight tick: re-run the counterparty-funding gate for proof-seen awaiting swaps
/// (rate-limited, no-op when idle) and drive + record + flood the money-path step for each that
/// advanced — exactly the middle third of [`crate::swap_node::gc_tick`], with the retransmit
/// drain, GC/refund sweep, match-window close, re-advertise, and beacon left OUT (slow-cadence
/// concerns; see the module docs). The mirror re-syncs only when something advanced, so an idle
/// fast tick touches nothing shared.
pub(crate) fn fast_tick(ctx: &WorkerCtx, st: &mut WorkerState) {
    let now = st.now_ms();
    // The combined work-exists + rate-limit gate (`fast_due`): an idle poll consults
    // nothing and burns no slot; a due one re-runs the funding gate for awaited swaps.
    let advanced = match st.swap.as_mut() {
        Some(session) => {
            if !session.fast_due(now) {
                return; // idle or rate-limited
            }
            session.reverify_awaited_funding()
        }
        None => return, // a relay-only node has no swaps to poll.
    };
    let head = ctx.cached_head().map(u64::from).unwrap_or(0);
    for swap_id in advanced {
        let replies = crate::swap_node::drive_phase_action(st, swap_id, head);
        if let (Some(session), Some(last)) = (st.swap.as_mut(), replies.last().cloned()) {
            session.record_action(swap_id, last.0, last.1);
        }
        for (mt, payload) in replies {
            crate::swap_node::flood_swap_reply(ctx, mt, payload, st);
        }
    }
    // Run-4 fix: the responder-claim ladder rides the fast beat too — the claim + its
    // confirmation are the LAST settlement leg, so leaving them slow-tick-only would put a
    // ~15 s quantum right back into the sub-30 s goal. Same gates, same code as the slow tick.
    crate::swap_claim::drive_responder_claims(ctx, st, head);
    crate::swap_mirror::sync_swap_phases(ctx, st);
}

// --- FFI surface (a second `#[uniffi::export]` block on MeshNode — the chat.rs pattern) ----------

#[uniffi::export]
impl MeshNode {
    /// The fast in-flight swap poll. The shim calls this on a ~3 s timer while a swap sheet is
    /// live; it re-runs ONLY the counterparty-funding re-verification (+ the resulting
    /// money-path step) so an awaited leg advances within seconds of reaching its depth /
    /// finality instead of waiting out the ~15 s maintenance beat. Rate-limited and idle-free
    /// in the core (see [`FAST_VERIFY_TICK_MS`]) — calling it more often, or on a node with no
    /// swap in flight, is a harmless no-op. Never emits beacons, retransmits, or GC.
    /// **Non-blocking (ADR-0002 gotcha a):** only enqueues; the worker does the chain reads.
    pub fn poll_swap_fast(&self) {
        self.enqueue_swap_fast_tick();
    }
}

#[cfg(test)]
#[path = "swap_fast_tick_tests.rs"]
mod tests;
