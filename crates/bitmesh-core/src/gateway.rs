//! # gateway — the `MeshGateway` seam + a record-only `MockGateway`
//!
//! A **gateway** is the one node with internet: it takes the opaque signed-tx wire off
//! the mesh and submits it to the Nimiq network, then emits a receipt back into the
//! mesh. G2 freezes the seam and ships a mock that **only records** submissions and
//! synthesizes a receipt — no network, no money path.
//!
//! G8: the real [`MeshGateway`] validates `networkId` + the validity window and calls
//! `sendRawTransaction(rawHex)` against a public Albatross **testnet** RPC. That is a
//! money-path goal, PR-only behind Andjroo — see the `// G8:` anchor in `submit`.

use std::sync::Mutex;

use crate::transport::{mock_tx_id, MeshError, TxId};
use crate::NetworkId;

/// A gateway's verdict on a submitted transaction, keyed by `tx_id`.
///
/// Honours core value #5 (unconfirmed-until-inclusion): only [`ReceiptStatus::Accepted`]
/// flips an origin to "settled"; [`ReceiptStatus::Expired`]/[`ReceiptStatus::Failed`]
/// surface as an explicit failed state rather than a silent drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The transaction this receipt acks.
    pub tx_id: TxId,
    /// The gateway's verdict.
    pub status: ReceiptStatus,
}

/// The outcome of a gateway submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptStatus {
    /// Accepted into the mempool (the only "settled" outcome).
    Accepted,
    /// Rejected as past its validity window.
    Expired,
    /// Rejected for any other reason (e.g. malformed, insufficient funds).
    Failed,
}

impl ReceiptStatus {
    /// The on-wire status byte carried in a receipt frame.
    pub const fn code(self) -> u8 {
        match self {
            ReceiptStatus::Accepted => 0,
            ReceiptStatus::Expired => 1,
            ReceiptStatus::Failed => 2,
        }
    }

    /// Parse a status byte back (unknown codes are treated as `Accepted`'s inverse —
    /// callers map any non-`Accepted` to a failed UI state).
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => ReceiptStatus::Expired,
            2 => ReceiptStatus::Failed,
            _ => ReceiptStatus::Accepted,
        }
    }
}

/// The frozen gateway seam: take opaque signed-tx bytes, return a receipt.
///
/// The relay/orchestrator depend only on this trait, so the G8 RPC gateway drops in
/// without touching the mesh layers.
pub trait MeshGateway: Send + Sync {
    /// Submit the opaque signed-tx wire; record/broadcast it and return a receipt.
    fn submit(&self, tx_wire: Vec<u8>) -> Result<Receipt, MeshError>;
}

/// A record-only [`MeshGateway`] for the mock pay-loop: no network, no money path.
///
/// It stores every submission (so a test can assert the gateway "broadcast" exactly
/// the opaque bytes the origin sent) and returns a receipt — `Accepted` by default, or
/// a forced status to exercise the expired/failed paths.
pub struct MockGateway {
    network: NetworkId,
    submissions: Mutex<Vec<Vec<u8>>>,
    forced_status: Mutex<ReceiptStatus>,
}

impl MockGateway {
    /// A mock gateway for the given network, accepting everything by default.
    pub fn new(network: NetworkId) -> Self {
        MockGateway {
            network,
            submissions: Mutex::new(Vec::new()),
            forced_status: Mutex::new(ReceiptStatus::Accepted),
        }
    }

    /// The network this gateway is configured for.
    pub fn network(&self) -> NetworkId {
        self.network
    }

    /// Force the status returned by future submissions (to test non-accepted paths).
    pub fn force_status(&self, status: ReceiptStatus) {
        *self.forced_status.lock().unwrap() = status;
    }

    /// How many submissions were recorded.
    pub fn submission_count(&self) -> usize {
        self.submissions.lock().unwrap().len()
    }

    /// A snapshot of the recorded submission payloads, in order.
    pub fn submissions(&self) -> Vec<Vec<u8>> {
        self.submissions.lock().unwrap().clone()
    }
}

impl MeshGateway for MockGateway {
    fn submit(&self, tx_wire: Vec<u8>) -> Result<Receipt, MeshError> {
        // G8: the real gateway validates networkId + validity window here, then calls
        //     `sendRawTransaction(rawHex)` against a public Albatross TESTNET RPC
        //     (money-path, gated). The mock only RECORDS the opaque bytes — it never
        //     touches the network and never inspects the payload's contents.
        let tx_id = mock_tx_id(&tx_wire);
        self.submissions.lock().unwrap().push(tx_wire);
        let status = *self.forced_status.lock().unwrap();
        Ok(Receipt { tx_id, status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_network;

    #[test]
    fn status_codes_roundtrip() {
        for s in [
            ReceiptStatus::Accepted,
            ReceiptStatus::Expired,
            ReceiptStatus::Failed,
        ] {
            assert_eq!(ReceiptStatus::from_code(s.code()), s);
        }
    }

    #[test]
    fn mock_gateway_records_and_acks() {
        let gw = MockGateway::new(default_network());
        assert_eq!(gw.network(), default_network());

        let wire = b"opaque-signed-tx".to_vec();
        let receipt = gw.submit(wire.clone()).unwrap();

        assert_eq!(receipt.status, ReceiptStatus::Accepted);
        assert_eq!(receipt.tx_id, mock_tx_id(&wire));
        assert_eq!(gw.submission_count(), 1);
        assert_eq!(gw.submissions()[0], wire);
    }

    #[test]
    fn forced_status_is_honoured() {
        let gw = MockGateway::new(default_network());
        gw.force_status(ReceiptStatus::Expired);
        assert_eq!(
            gw.submit(b"x".to_vec()).unwrap().status,
            ReceiptStatus::Expired
        );
    }
}
