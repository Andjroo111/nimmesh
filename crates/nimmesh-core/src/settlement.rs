//! # settlement — G17 two-way settlement closure (sender *and* receiver)
//!
//! "Did it land?" is the offline-pay question for **both** parties. The sender wants to know
//! their payment was accepted; the receiver wants to know money they were expecting actually
//! arrived. G8 already floods a `nimiqTxReceipt` (`0x31`) when a gateway accepts (or rejects)
//! a tx, and G7 store-and-forward catches a rejoining node up on it — so the receipt reaches
//! both sides even if neither was online at submit time.
//!
//! This module owns the per-node **settlement ledger** that closes that loop. It is the
//! origin-payment tracking lifted out of [`crate::engine`] (keeping that file under the
//! 800-line guard) and generalised to two directions:
//!
//! - **Outgoing** — recorded when this node submits a tx (`MeshNode::submit_*`).
//! - **Incoming** — recorded when this node is the payee and registers the txId of a payment
//!   it expects (`MeshNode::watch_incoming`, learned via the request/confirmation flow).
//!
//! The **same** flooded receipt settles whichever side is watching that txId, so a `Pending →
//! ✓ Settled` (or `✗ Failed` on a reject/expire NACK) closes for sender and receiver alike.
//! Non-money-path: it tracks public `txId`s and the public receipt status only — no keys, no
//! payload inspection, and the relay stays blind (a node only matches receipts to txIds it
//! itself registered; it never parses a tx to guess who it is for, preserving trustless relay).

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::gateway::ReceiptStatus;
use crate::transport::TxId;

/// A `nimiqTxReceipt` payload is exactly `txId(32) | status(1)`.
pub(crate) const RECEIPT_PAYLOAD_LEN: usize = 33;

/// Encode a `nimiqTxReceipt` payload: `txId(32) | status(1)`. (Receipt codec lives here
/// with the ledger it feeds — moved from `engine.rs` for the 800-line guard.)
pub(crate) fn encode_receipt(tx_id: &TxId, status: ReceiptStatus) -> Vec<u8> {
    let mut v = Vec::with_capacity(RECEIPT_PAYLOAD_LEN);
    v.extend_from_slice(&tx_id.0);
    v.push(status.code());
    v
}

/// Decode a `nimiqTxReceipt` payload, returning `None` on any malformed input.
pub(crate) fn decode_receipt(payload: &[u8]) -> Option<(TxId, ReceiptStatus)> {
    if payload.len() != RECEIPT_PAYLOAD_LEN {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&payload[..32]);
    Some((TxId(id), ReceiptStatus::from_code(payload[32])))
}

/// Where a payment stands. `Pending` until a gateway receipt arrives; honours
/// unconfirmed-until-inclusion (core value #5) — only an `Accepted` receipt yields `Settled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PaymentStatus {
    /// Sent / expected / relaying; no gateway receipt seen yet.
    Pending,
    /// A gateway accepted the tx into the mempool.
    Settled,
    /// A gateway rejected the tx (expired / failed).
    Failed,
}

/// Which side of a payment a node is tracking — so the UI can say "your payment landed" vs
/// "you got paid" from the one shared receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SettlementDirection {
    /// This node sent the payment (the origin): "did my payment land?"
    Outgoing,
    /// This node is the payee watching for an expected payment: "did I get paid?"
    Incoming,
}

/// A tracked payment's closure state — its status plus which side this node is on (FFI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Settlement {
    /// Pending until a receipt arrives, then Settled / Failed.
    pub status: PaymentStatus,
    /// Whether this node sent it (Outgoing) or is awaiting it as the payee (Incoming).
    pub direction: SettlementDirection,
}

#[derive(Clone, Copy)]
struct Tracked {
    status: PaymentStatus,
    direction: SettlementDirection,
}

/// A node's ledger of in-flight payments + the settle signal. Both the sender (Outgoing, via
/// submit) and the receiver (Incoming, via `watch_incoming`) record a txId here; the same
/// flooded receipt settles whichever side is watching it.
pub(crate) struct SettlementLedger {
    payments: Mutex<HashMap<TxId, Tracked>>,
    settled: Condvar,
}

impl SettlementLedger {
    pub(crate) fn new() -> Self {
        SettlementLedger {
            payments: Mutex::new(HashMap::new()),
            settled: Condvar::new(),
        }
    }

    /// Record interest in `tx_id` as `Pending` in `direction`. Idempotent: it never overwrites
    /// an already-tracked entry (so a settled payment can't be reset to pending, and the first
    /// recorded direction wins).
    pub(crate) fn record(&self, tx_id: TxId, direction: SettlementDirection) {
        self.payments
            .lock()
            .unwrap()
            .entry(tx_id)
            .or_insert(Tracked {
                status: PaymentStatus::Pending,
                direction,
            });
    }

    /// Apply a gateway receipt: flip a tracked `Pending` entry to `status` and wake any waiter.
    /// A no-op for an untracked txId or one already terminal (idempotent under receipt echoes).
    pub(crate) fn settle(&self, tx_id: TxId, status: PaymentStatus) {
        let mut g = self.payments.lock().unwrap();
        if let Some(t) = g.get_mut(&tx_id) {
            if t.status == PaymentStatus::Pending {
                t.status = status;
                drop(g);
                self.settled.notify_all();
            }
        }
    }

    /// The status of `tx_id` (or `Pending` if untracked).
    pub(crate) fn status(&self, tx_id: &TxId) -> PaymentStatus {
        self.payments
            .lock()
            .unwrap()
            .get(tx_id)
            .map(|t| t.status)
            .unwrap_or(PaymentStatus::Pending)
    }

    /// The full closure state (status + direction) of `tx_id`, or `None` if this node isn't
    /// tracking it.
    pub(crate) fn settlement(&self, tx_id: &TxId) -> Option<Settlement> {
        self.payments
            .lock()
            .unwrap()
            .get(tx_id)
            .map(|t| Settlement {
                status: t.status,
                direction: t.direction,
            })
    }

    /// Block (up to `timeout`) until `tx_id` leaves `Pending`, returning the final status (or
    /// the last-known status on timeout).
    #[cfg(test)]
    pub(crate) fn wait(&self, tx_id: TxId, timeout: Duration) -> PaymentStatus {
        let deadline = Instant::now() + timeout;
        let mut guard = self.payments.lock().unwrap();
        loop {
            let status = guard
                .get(&tx_id)
                .map(|t| t.status)
                .unwrap_or(PaymentStatus::Pending);
            if status != PaymentStatus::Pending {
                return status;
            }
            let now = Instant::now();
            if now >= deadline {
                return status;
            }
            let (g, _) = self.settled.wait_timeout(guard, deadline - now).unwrap();
            guard = g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(n: u8) -> TxId {
        TxId([n; 32])
    }

    #[test]
    fn an_outgoing_payment_settles_on_its_receipt() {
        let l = SettlementLedger::new();
        l.record(tx(1), SettlementDirection::Outgoing);
        assert_eq!(l.status(&tx(1)), PaymentStatus::Pending);
        l.settle(tx(1), PaymentStatus::Settled);
        assert_eq!(l.status(&tx(1)), PaymentStatus::Settled);
        assert_eq!(
            l.settlement(&tx(1)).unwrap().direction,
            SettlementDirection::Outgoing
        );
    }

    #[test]
    fn an_incoming_payment_settles_on_the_same_receipt() {
        // The receiver watches a txId it expects; the gateway's receipt closes it too.
        let l = SettlementLedger::new();
        l.record(tx(2), SettlementDirection::Incoming);
        l.settle(tx(2), PaymentStatus::Settled);
        let s = l.settlement(&tx(2)).unwrap();
        assert_eq!(s.status, PaymentStatus::Settled);
        assert_eq!(s.direction, SettlementDirection::Incoming);
    }

    #[test]
    fn a_reject_receipt_fails_either_direction() {
        let l = SettlementLedger::new();
        l.record(tx(3), SettlementDirection::Outgoing);
        l.record(tx(4), SettlementDirection::Incoming);
        l.settle(tx(3), PaymentStatus::Failed);
        l.settle(tx(4), PaymentStatus::Failed);
        assert_eq!(l.status(&tx(3)), PaymentStatus::Failed);
        assert_eq!(l.status(&tx(4)), PaymentStatus::Failed);
    }

    #[test]
    fn settle_is_idempotent_and_record_never_downgrades() {
        let l = SettlementLedger::new();
        l.record(tx(5), SettlementDirection::Outgoing);
        l.settle(tx(5), PaymentStatus::Settled);
        // A later duplicate receipt (Failed) must not override an already-Settled payment.
        l.settle(tx(5), PaymentStatus::Failed);
        assert_eq!(l.status(&tx(5)), PaymentStatus::Settled);
        // A late re-record must not reset a settled payment to pending, nor flip its direction.
        l.record(tx(5), SettlementDirection::Incoming);
        let s = l.settlement(&tx(5)).unwrap();
        assert_eq!(s.status, PaymentStatus::Settled);
        assert_eq!(s.direction, SettlementDirection::Outgoing);
    }

    #[test]
    fn untracked_is_pending_and_has_no_settlement() {
        let l = SettlementLedger::new();
        assert_eq!(l.status(&tx(9)), PaymentStatus::Pending);
        assert!(l.settlement(&tx(9)).is_none());
        // Settling an untracked txId is a harmless no-op.
        l.settle(tx(9), PaymentStatus::Settled);
        assert!(l.settlement(&tx(9)).is_none());
    }

    #[test]
    fn receipt_payload_round_trips() {
        let id = crate::transport::mock_tx_id(b"hello");
        for status in [
            ReceiptStatus::Accepted,
            ReceiptStatus::Expired,
            ReceiptStatus::Failed,
        ] {
            let bytes = encode_receipt(&id, status);
            assert_eq!(bytes.len(), RECEIPT_PAYLOAD_LEN);
            let (back_id, back_status) = decode_receipt(&bytes).unwrap();
            assert_eq!(back_id, id);
            assert_eq!(back_status, status);
        }
    }

    #[test]
    fn decode_receipt_rejects_wrong_length() {
        assert!(decode_receipt(&[]).is_none());
        assert!(decode_receipt(&[0u8; 32]).is_none());
        assert!(decode_receipt(&[0u8; 34]).is_none());
    }
}
