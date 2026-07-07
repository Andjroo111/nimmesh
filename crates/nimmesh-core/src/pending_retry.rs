//! # pending_retry — the data-mule: re-offer your own unsettled txs until they expire
//!
//! Andjroo's drive-away scenario: sign a payment far from any peer, drive home, and the
//! payment should deliver itself the moment the mesh reappears. Before this module the
//! origin flooded its tx exactly ONCE; with no peers in range the bytes only lived in
//! the G7 recent-cache (15-minute retention), so a longer round trip lost the send even
//! though the signed tx stays chain-valid for ~2 hours (RISKS.md #1).
//!
//! Now the ORIGIN keeps every still-pending tx it originated (bytes + `validUntil`) and
//! re-floods it on the periodic tick while it has peers. Re-flooding the SAME packet
//! bytes is idempotent and cheap: peers that already saw it drop it at the blind dedup
//! (same relay key), gateways dedup by txId — only a NEWLY-met peer actually carries it.
//!
//! Honesty at the end of the window: once the freshest heard head is past `validUntil`
//! (the chain can no longer include the tx), the entry is dropped and the payment is
//! settled **Failed** locally, so the UI stops saying "relaying" about a dead send. A tx
//! with no head heard at signing (no `validUntil`) retries on a wall-clock bound of the
//! same ~2 h window instead.
//!
//! Non-money-path: carries only the already-signed, already-public packet bytes.

use crate::beacon::is_expired;
use crate::engine::{WorkerCtx, WorkerState};
use crate::settlement::PaymentStatus;
use crate::transport::TxId;

/// Upper bound on remembered own-pending txs (a phone sends a handful, not hundreds).
pub const MAX_PENDING: usize = 16;

/// Re-flood cadence: at most once per this many ms per tx (the tick itself runs ~15 s).
pub const RETRY_MS: u64 = 15_000;

/// Wall-clock retry bound for a tx signed with NO head heard (`validUntil` unknown):
/// the Albatross validity window is ~7200 blocks ≈ 2 h — past that it cannot land.
pub const RETRY_WALL_MS: u64 = 2 * 60 * 60 * 1000;

struct PendingTx {
    tx_id: TxId,
    bytes: Vec<u8>,
    valid_until: Option<u32>,
    created_ms: u64,
    last_flood_ms: u64,
}

/// The origin's bounded set of still-pending own txs (worker-thread state).
#[derive(Default)]
pub(crate) struct PendingRetry {
    entries: Vec<PendingTx>,
}

impl PendingRetry {
    /// Remember an own tx just flooded (drops the oldest beyond [`MAX_PENDING`]).
    /// `delivered_to_someone` = whether any peer was connected for the initial flood; a
    /// tx flooded into the void retries IMMEDIATELY on the first tick that has peers
    /// (the drive-away case), instead of waiting out the [`RETRY_MS`] cadence.
    pub(crate) fn remember(
        &mut self,
        tx_id: TxId,
        bytes: Vec<u8>,
        valid_until: Option<u32>,
        now_ms: u64,
        delivered_to_someone: bool,
    ) {
        self.entries.retain(|e| e.tx_id != tx_id);
        if self.entries.len() >= MAX_PENDING {
            self.entries.remove(0);
        }
        self.entries.push(PendingTx {
            tx_id,
            bytes,
            valid_until,
            created_ms: now_ms,
            last_flood_ms: if delivered_to_someone { now_ms } else { 0 },
        });
    }

    /// A receipt (any status) settled this tx — stop retrying it.
    pub(crate) fn clear(&mut self, tx_id: &TxId) {
        self.entries.retain(|e| e.tx_id != *tx_id);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The periodic retry pass (runs on the ~15 s beacon/sync ticks): drop-and-fail expired
/// entries; re-flood live ones (rate-limited per tx) while any peer is connected.
pub(crate) fn tick(ctx: &WorkerCtx, st: &mut WorkerState) {
    let now = st.now_ms();
    let head = ctx.cached_head();
    let has_peers = ctx.peer_degree() > 0;
    let mut expired: Vec<TxId> = Vec::new();
    st.pending.entries.retain(|e| {
        let dead = match e.valid_until {
            Some(vu) => is_expired(head, Some(vu)),
            None => now.saturating_sub(e.created_ms) > RETRY_WALL_MS,
        };
        if dead {
            expired.push(e.tx_id);
        }
        !dead
    });
    for tx_id in expired {
        // The window is closed — the chain can no longer include it. Fail it honestly.
        ctx.settle(tx_id, PaymentStatus::Failed);
    }
    if !has_peers {
        return;
    }
    for e in st.pending.entries.iter_mut() {
        if now.saturating_sub(e.last_flood_ms) >= RETRY_MS {
            e.last_flood_ms = now;
            // Same bytes, same relay key: peers that saw it dedup; new peers carry it.
            ctx.flood(e.bytes.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> TxId {
        TxId([b; 32])
    }

    #[test]
    fn remember_is_bounded_and_deduped() {
        let mut p = PendingRetry::default();
        for i in 0..(MAX_PENDING as u8 + 4) {
            p.remember(id(i), vec![i], None, 0, true);
        }
        assert_eq!(p.len(), MAX_PENDING);
        p.remember(id(200), vec![1], None, 0, true);
        p.remember(id(200), vec![2], None, 0, true); // replaces, not duplicates
        assert_eq!(p.len(), MAX_PENDING);
    }

    #[test]
    fn clear_removes_a_settled_tx() {
        let mut p = PendingRetry::default();
        p.remember(id(1), vec![1], None, 0, true);
        p.remember(id(2), vec![2], None, 0, true);
        p.clear(&id(1));
        assert_eq!(p.len(), 1);
    }
}
