//! # swap_node_test_hooks — the `#[cfg(test)]`-only observability + origination hooks of
//! swap_node, extracted so the production module stays under the 800-line ceiling. A CHILD
//! module (`#[path]`), not a `swap_*_tests` sibling: these helpers read `WorkerCtx`/
//! `WorkerState`/`IntentMetrics` internals that stay private to the swap_node subtree, and
//! `swap_node` re-exports them so every caller keeps its `crate::swap_node::…` path. The whole
//! file only compiles under `cfg(test)` (the `mod` declaration carries the attribute).

use super::*;

/// A point-in-time read of [`IntentMetrics`] (G42, test/observability). Internal counts stay `usize`
/// so [`crate::swap_health::discovery_health`] can do its arithmetic directly; the FFI boundary uses
/// the `u64` [`FfiIntentMetrics`] mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IntentMetricsSnapshot {
    pub(crate) seen: usize,
    pub(crate) matched: usize,
    pub(crate) dropped_rate: usize,
    pub(crate) dropped_expiry: usize,
    pub(crate) dropped_throttle: usize,
    pub(crate) dropped_signature: usize,
    pub(crate) readvertised: usize,
}
impl super::IntentMetrics {
    /// A consistent read of all counters (G42, test/health).
    pub(crate) fn snapshot(&self) -> IntentMetricsSnapshot {
        let g = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        IntentMetricsSnapshot {
            seen: g(&self.seen),
            matched: g(&self.matched),
            dropped_rate: g(&self.dropped_rate),
            dropped_expiry: g(&self.dropped_expiry),
            dropped_throttle: g(&self.dropped_throttle),
            dropped_signature: g(&self.dropped_signature),
            readvertised: g(&self.readvertised),
        }
    }
}
/// This node's current phase for a swap it participates in (test/observability hook).
pub(crate) fn swap_phase(
    ctx: &WorkerCtx,
    swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
) -> Option<crate::swap::SwapPhase> {
    ctx.swaps.lock().unwrap().get(&swap_id).copied()
}

/// G14 (test origination): register an initiator coordinator this node started and flood its
/// `Propose`. The real money-path origination API (build the funding txs, sign offline) is a later,
/// gated goal — this drives the negotiation half over the real node loop.
pub(crate) fn start_swap(
    ctx: &WorkerCtx,
    swap_id: [u8; crate::swap_wire::SWAP_ID_LEN],
    coordinator: crate::swap_coordinator::SwapCoordinator,
    propose: crate::swap_wire::SwapEnvelope,
    st: &mut WorkerState,
) {
    let Some(session) = st.swap.as_mut() else {
        return; // not a participant — nothing to originate.
    };
    session.add_initiator(swap_id, coordinator);
    sync_swap_phases(ctx, st);
    if let Ok(payload) = crate::swap_wire::encode_swap(&propose) {
        // G20: remember the Propose so the tick retransmits it if the first flood is lost.
        if let Some(session) = st.swap.as_mut() {
            session.record_action(swap_id, MessageType::SwapPropose, payload.clone());
        }
        flood_swap_reply(ctx, MessageType::SwapPropose, payload, st);
    }
}
