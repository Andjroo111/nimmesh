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
    ctx.swaps.lock().unwrap().get(&swap_id).map(|(p, _)| *p)
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
    crate::swap_mirror::sync_swap_phases(ctx, st);
    if let Ok(payload) = crate::swap_wire::encode_swap(&propose) {
        // G20: remember the Propose so the tick retransmits it if the first flood is lost.
        if let Some(session) = st.swap.as_mut() {
            session.record_action(swap_id, MessageType::SwapPropose, payload.clone());
        }
        flood_swap_reply(ctx, MessageType::SwapPropose, payload, st);
    }
}

// --- M3 (G8 review): the driver's reveal-deadline pre-gate --------------------------------------

/// A spy signer counting `build_claim` calls — the M3 gate must keep `S` away from the
/// signer ENTIRELY when the deadline is too close (a live claim broadcasts `withdraw(S)`,
/// so even one call would be the on-chain reveal).
struct ClaimSpy {
    claims: std::sync::Arc<AtomicUsize>,
}

impl crate::swap_signer::SwapSigner for ClaimSpy {
    fn build_funding(
        &self,
        _ctx: &crate::swap_coordinator::SwapContext,
        _leg: crate::swap_wire::SwapLegId,
    ) -> Option<(Vec<u8>, [u8; 32])> {
        None
    }
    fn build_claim(
        &self,
        ctx: &crate::swap_coordinator::SwapContext,
        secret: [u8; 32],
    ) -> Option<(Vec<u8>, [u8; 32])> {
        self.claims.fetch_add(1, Ordering::SeqCst);
        Some((
            secret.to_vec(),
            crate::swap_signer::sim_tx_id(ctx.swap_id, 0xF3),
        ))
    }
}

#[test]
fn the_driver_holds_the_claim_when_the_reveal_deadline_is_too_close() {
    use crate::swap::{LadderParams, SwapTerms};
    use crate::swap_coordinator::{SwapContext, SwapCoordinator};
    use crate::swap_messages::{tx_envelope, SwapAcceptance};
    use crate::swap_rate::RatePolicy;
    use crate::swap_session::{NodeIdentity, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS};
    use crate::swap_wire::SwapLegId;

    let swap_id = [0x7C; 16];
    let secret = [42u8; 32];
    let btc_pk = {
        let mut k = [0x11; 33];
        k[0] = 0x02;
        k
    };
    // An initiator at BothFunded (T_B = 5_000, default 1_800-block claim window), wrapped
    // in a WorkerState so the REAL driver runs.
    let build_state = |claims: std::sync::Arc<AtomicUsize>| {
        let ctx = SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: crate::swap_leg::sha256(&secret),
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: btc_pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
            term_anchor: 0,
        };
        let (mut coord, _propose) =
            SwapCoordinator::new_initiator(ctx, secret, LadderParams::default());
        coord
            .recv_accept(
                &SwapAcceptance {
                    swap_id,
                    nim_address: [0xB2; 20],
                    btc_address: b"tb1qbob".to_vec(),
                    btc_pubkey: [0x03; 33],
                }
                .to_envelope(),
                0,
            )
            .unwrap();
        coord.fund(0, vec![0x11; 248], [0xC1; 32]).unwrap();
        coord
            .recv_funding_proof(&tx_envelope(
                swap_id,
                SwapLegId::Counterparty,
                vec![0x22; 120],
                [0xF2; 32],
            ))
            .unwrap();
        assert_eq!(coord.phase(), crate::swap::SwapPhase::BothFunded);
        let identity = NodeIdentity {
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: btc_pk,
            rate_policy: RatePolicy::accept_all(),
            max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
            standing_intent: None,
        };
        let mut session = SwapSession::new(identity, LadderParams::default());
        session.add_initiator(swap_id, coord);
        WorkerState::new(
            crate::relay::RelayPolicy::deterministic(),
            false,
            Some(session),
            Some(Box::new(ClaimSpy { claims })),
        )
    };
    let payload = crate::swap_wire::encode_swap(&tx_envelope(
        swap_id,
        SwapLegId::Counterparty,
        vec![0x22; 120],
        [0xF2; 32],
    ))
    .unwrap();

    // Head inside the claim window of T_B (5_000 − 3_201 = 1_799 < 1_800): S never reaches
    // the signer, nothing floods.
    let claims = std::sync::Arc::new(AtomicUsize::new(0));
    let mut st = build_state(claims.clone());
    let out = super::drive_swap(
        &mut st,
        crate::swap_wire::SwapKind::FundingProof,
        &payload,
        3_201,
    );
    assert!(out.is_empty());
    assert_eq!(
        claims.load(Ordering::SeqCst),
        0,
        "the M3 gate must hold S back from the signer"
    );

    // A safe head claims + reveals normally through the same driver.
    let claims = std::sync::Arc::new(AtomicUsize::new(0));
    let mut st = build_state(claims.clone());
    let out = super::drive_swap(
        &mut st,
        crate::swap_wire::SwapKind::FundingProof,
        &payload,
        1_000,
    );
    assert_eq!(claims.load(Ordering::SeqCst), 1);
    assert_eq!(out.len(), 1, "the PreimageReveal envelope floods");
}
