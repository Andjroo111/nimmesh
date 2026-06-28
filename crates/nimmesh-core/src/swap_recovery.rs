//! # swap_recovery — crash recovery of in-flight swaps (G31)
//!
//! A [`crate::swap_session::SwapSession`] lives only in memory, so a node that crashes with funds
//! locked would lose its coordinator and, with it, the refund path. This module gives the session a
//! [`snapshot`](SwapSession::snapshot) / [`restore`](SwapSession::restore) pair: the node persists the
//! per-swap [`CoordinatorSnapshot`]s and rebuilds the session on restart, so the refund tick re-arms
//! and "worst case is a refund" holds across a crash, not just within one process.
//!
//! The snapshot carries the initiator secret (it must, to rebuild an initiator). The node is
//! responsible for storing it as securely as a key; `CoordinatorSnapshot` has no `Debug` so it is
//! never logged. The pending retransmit buffer is intentionally not captured: a restarted node
//! re-derives it as the swap advances, and the refund/GC tick needs only the coordinators.

use crate::swap::LadderParams;
use crate::swap_coordinator::{CoordinatorSnapshot, SwapCoordinator};
use crate::swap_session::{NodeIdentity, SwapSession};

impl SwapSession {
    /// Capture every in-flight swap for crash recovery. The node persists these (securely — they
    /// carry the initiator secret) and feeds them back to [`restore`](Self::restore) on restart.
    pub fn snapshot(&self) -> Vec<CoordinatorSnapshot> {
        self.coordinators
            .values()
            .map(|c| c.to_snapshot())
            .collect()
    }

    /// Rebuild a session from `identity` + `ladder` + a persisted snapshot, so its refund/GC tick
    /// resumes after a restart. The rate/concurrency policies come from `identity`; the pending
    /// retransmit buffer starts empty (re-derived as swaps advance).
    pub fn restore(
        identity: NodeIdentity,
        ladder: LadderParams,
        snapshot: Vec<CoordinatorSnapshot>,
    ) -> Self {
        let mut session = SwapSession::new(identity, ladder);
        for snap in snapshot {
            let swap_id = snap.ctx.swap_id;
            session
                .coordinators
                .insert(swap_id, SwapCoordinator::from_snapshot(snap, ladder));
        }
        session
    }
}

#[cfg(test)]
mod tests {
    use crate::swap::{LadderParams, SwapPhase, SwapTerms};
    use crate::swap_coordinator::{SwapContext, SwapCoordinator};
    use crate::swap_leg::sha256;
    use crate::swap_messages::SwapAcceptance;
    use crate::swap_session::{
        NodeIdentity, RatePolicy, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS,
    };

    fn identity() -> NodeIdentity {
        let mut pk = [0x22; 33];
        pk[0] = 0x02;
        NodeIdentity {
            nim_address: [0xB2; 20],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: pk,
            rate_policy: RatePolicy::accept_all(),
            max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
        }
    }

    #[test]
    fn a_funds_locked_swap_survives_a_snapshot_restore_and_still_refunds() {
        // G31: a node funds its NIM leg, then "crashes". Its session is snapshotted, dropped, and
        // restored into a fresh session — and the restored coordinator can still refund past its
        // timeout. "Worst case is a refund" survives the restart, not just one process.
        let swap_id = [0x5A; 16];
        let secret = [42u8; 32];
        let ladder = LadderParams::default();
        let mut alice_pk = [0x11; 33];
        alice_pk[0] = 0x02;
        let ctx = SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: sha256(&secret),
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: alice_pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
        };

        // Drive alice's initiator coordinator to SelfFunded (her NIM leg is locked).
        let (mut coord, _propose) = SwapCoordinator::new_initiator(ctx, secret, ladder);
        let accept = SwapAcceptance {
            swap_id,
            nim_address: [0xB2; 20],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: [0x03; 33],
        }
        .to_envelope();
        coord.recv_accept(&accept, 0).unwrap();
        coord.fund(0, vec![0x11; 248], [0xC1; 32]).unwrap();
        assert!(coord.phase().has_funds_locked());

        // Put it in a session, snapshot, then "crash" (drop the session).
        let mut before = SwapSession::new(identity(), ladder);
        before.add_initiator(swap_id, coord);
        let snapshot = before.snapshot();
        assert_eq!(snapshot.len(), 1);
        drop(before);

        // Restart: rebuild from the snapshot. The funds-locked swap is back, at the same phase.
        let mut after = SwapSession::restore(identity(), ladder, snapshot);
        assert!(after
            .coordinator(&swap_id)
            .unwrap()
            .phase()
            .has_funds_locked());

        // The restored coordinator still refunds past its timeout, and the GC tick reaps it.
        assert_eq!(
            after
                .coordinator(&swap_id)
                .unwrap()
                .refund_after_timeout(10_001),
            Ok(())
        );
        assert_eq!(
            after.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Refunded
        );
        after.tick(10_001);
        assert!(after.coordinator(&swap_id).is_none());
    }

    #[test]
    fn the_worker_gc_tick_refunds_a_restored_funds_locked_swap() {
        // The same, but let the GC/refund tick (not a manual refund) drive it — exactly what a
        // restarted node's maintenance tick does.
        let swap_id = [0x5B; 16];
        let secret = [7u8; 32];
        let ladder = LadderParams::default();
        let mut alice_pk = [0x11; 33];
        alice_pk[0] = 0x02;
        let ctx = SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: sha256(&secret),
            nim_address: [0xA1; 20],
            btc_address: b"tb1qalice".to_vec(),
            btc_pubkey: alice_pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
        };
        let (mut coord, _p) = SwapCoordinator::new_initiator(ctx, secret, ladder);
        let accept = SwapAcceptance {
            swap_id,
            nim_address: [0xB2; 20],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: [0x03; 33],
        }
        .to_envelope();
        coord.recv_accept(&accept, 0).unwrap();
        coord.fund(0, vec![0x11; 248], [0xC1; 32]).unwrap();

        let mut before = SwapSession::new(identity(), ladder);
        before.add_initiator(swap_id, coord);
        let mut after = SwapSession::restore(identity(), ladder, before.snapshot());

        // Before the timeout the restored swap is kept (no premature refund); past it, the tick
        // refunds + reaps it — proving the safety exit resumes after a restart.
        assert!(after.tick(5_000).is_empty());
        assert_eq!(after.len(), 1);
        after.tick(10_001);
        assert!(after.is_empty());
    }
}
