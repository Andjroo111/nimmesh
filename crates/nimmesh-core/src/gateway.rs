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

use std::sync::Arc;
use std::sync::Mutex;

use crate::beacon::HeadBeacon;
use crate::nimiq::hex::bytes_to_hex;
use crate::rpc::GatewayRpc;
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

/// The decoded envelope context the engine hands a gateway alongside the opaque wire, so
/// the gateway can validate `networkId` + the validity window before broadcasting without
/// re-parsing the TLV. The `tx_id` is the SAME key the origin tracks its payment by, so
/// the receipt the gateway returns correlates back to the right pending payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitContext {
    /// The receipt/dedup key (origin's `txId` if present, else the mock derivation).
    pub tx_id: TxId,
    /// The opaque signed-tx wire bytes (broadcast verbatim as hex; never inspected here).
    pub tx_wire: Vec<u8>,
    /// The envelope's `networkId` — the gateway drops a mismatch (PROTOCOL.md §gateway).
    pub network_id: u8,
    /// The envelope's optional `validUntil` height — the gateway drops if `head >= validUntil`.
    pub valid_until: Option<u32>,
}

/// The frozen gateway seam: take opaque signed-tx bytes, return a receipt.
///
/// The relay/orchestrator depend only on this trait, so the G8 RPC gateway drops in
/// without touching the mesh layers.
pub trait MeshGateway: Send + Sync {
    /// Submit the opaque signed-tx wire; record/broadcast it and return a receipt.
    fn submit(&self, tx_wire: Vec<u8>) -> Result<Receipt, MeshError>;

    /// Submit with the decoded envelope context (`networkId` + validity window + `txId`).
    ///
    /// The default delegates to [`MeshGateway::submit`] and keys the receipt by `ctx.tx_id`
    /// — the record-only [`MockGateway`] keeps its exact behaviour. The real
    /// [`RpcGateway`] overrides this to enforce the testnet `networkId`, drop an expired tx
    /// (querying the live head), and call `sendRawTransaction(rawHex)`.
    fn submit_validated(&self, ctx: SubmitContext) -> Result<Receipt, MeshError> {
        let receipt = self.submit(ctx.tx_wire)?;
        Ok(Receipt {
            tx_id: ctx.tx_id,
            status: receipt.status,
        })
    }

    /// G9: this gateway's current chain head, for the `nimiqHeadBeacon` (`0x32`) emit.
    ///
    /// `None` for a gateway with no live chain view (the record-only [`MockGateway`] by
    /// default). The real [`RpcGateway`] sources the height from its RPC `block_number`.
    /// The engine floods the returned beacon so deep-offline signers anchor a fresh head.
    fn head_beacon(&self) -> Option<HeadBeacon> {
        None
    }

    /// G15: answer a mesh balance query — the public on-chain balance for `address` (the
    /// user-friendly `NQ…` form), read at the gateway's current head. `None` for a node with
    /// no live chain view (the record-only [`MockGateway`] by default, unless a test sets one)
    /// or on a transient RPC failure (emit nothing rather than a wrong/stale balance). The
    /// engine floods the returned answer as a `nimiqBalanceResponse` (`0x34`). **Read-only**:
    /// public state only, never key material (non-money-path).
    fn balance_of(&self, address: &str) -> Option<BalanceAnswer> {
        let _ = address;
        None
    }

    /// Answer a mesh transaction-history query (`0x35`): the recent txs for `address`
    /// read at the gateway's current head, shaped as the compact mesh response. `None`
    /// for a node with no live chain view or on a transient RPC failure (emit nothing
    /// rather than wrong/stale rows). **Read-only** public chain data — no keys.
    fn history_of(&self, address: &str) -> Option<crate::tx_history::HistoryResponse> {
        let _ = address;
        None
    }
}

/// A gateway's answer to a balance query (G15): the balance it read for the address, the head
/// height it read at (the freshness anchor), and the network it is on. **Unverified** by the
/// receiving node until a future accounts-proof binds it to the head-beacon hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceAnswer {
    /// Balance in luna (1 NIM = 100_000 luna).
    pub balance: u64,
    /// The chain head height the balance was read at.
    pub head_height: u32,
    /// The Albatross network-id byte the gateway is on (testnet `5`).
    pub network_id: u8,
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
    /// G15: an optional `(balance, head_height)` this mock answers balance queries with.
    /// `None` (default) means it answers nothing — like a gateway with no chain view.
    balance: Mutex<Option<(u64, u32)>>,
}

impl MockGateway {
    /// A mock gateway for the given network, accepting everything by default.
    pub fn new(network: NetworkId) -> Self {
        MockGateway {
            network,
            submissions: Mutex::new(Vec::new()),
            forced_status: Mutex::new(ReceiptStatus::Accepted),
            balance: Mutex::new(None),
        }
    }

    /// G15: make this mock answer balance queries with `balance` luna at `head_height`
    /// (on its configured network). Lets a mesh test exercise the query→answer→cache loop.
    pub fn set_balance(&self, balance: u64, head_height: u32) {
        *self.balance.lock().unwrap() = Some((balance, head_height));
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
    /// G15: answer with the configured `(balance, head_height)` (if any) on this gateway's
    /// network. Records nothing and reads nothing real — purely the test-injected value.
    fn balance_of(&self, _address: &str) -> Option<BalanceAnswer> {
        let (balance, head_height) = (*self.balance.lock().unwrap())?;
        Some(BalanceAnswer {
            balance,
            head_height,
            network_id: self.network.wire_id(),
        })
    }

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

// --- RpcGateway: the real online hop (G8, money-path, TESTNET-only) ------------------

/// The real [`MeshGateway`]: validate then broadcast a signed Nimiq tx over a
/// [`GatewayRpc`]. This is the one place the mesh touches the internet (PROTOCOL.md
/// §"Gateway broadcast"). It is generic over the RPC seam, so the wiring + validity-window
/// logic is fully unit-tested offline against [`crate::rpc::MockRpc`]; the live example
/// injects the feature-gated `HttpGatewayRpc`.
///
/// On a `nimiqTx` it:
/// 1. **guards `networkId`** — anything but the configured testnet id is a [`ReceiptStatus::Failed`];
/// 2. **checks the validity window** — fetches the head height and, if `head >= validUntil`,
///    returns [`ReceiptStatus::Expired`] WITHOUT broadcasting (RISKS.md #1);
/// 3. **broadcasts** `sendRawTransaction(rawHex)` — a terminal RPC rejection is
///    [`ReceiptStatus::Failed`]; acceptance is [`ReceiptStatus::Accepted`];
/// 4. a **transient** RPC/transport error surfaces as `Err(MeshError::Gateway)` so the
///    engine emits NO receipt and another gateway / a retry can still carry the tx.
///
/// **Testnet-only:** the `network_id` it enforces is fixed to [`NetworkId::Testnet`] at
/// construction, and the injected RPC client is itself testnet-guarded.
pub struct RpcGateway {
    rpc: Arc<dyn GatewayRpc>,
    /// The Albatross network-id byte this gateway accepts (always testnet `5`).
    network_id: u8,
}

impl RpcGateway {
    /// Build a testnet gateway over an injected RPC client. The enforced `networkId` is
    /// fixed to [`NetworkId::Testnet`] — the only mainnet path is the loudly-separate,
    /// Andjroo-gated [`RpcGateway::new_mainnet`].
    pub fn new(rpc: Arc<dyn GatewayRpc>) -> Self {
        RpcGateway {
            rpc,
            network_id: NetworkId::Testnet.wire_id(),
        }
    }

    /// **OWNER-GATED (real money): a MAINNET gateway.** Enforces `networkId = 24` on
    /// every tx it hears — a testnet-signed tx is refused, exactly mirroring how the
    /// testnet gateway refuses mainnet bytes. Exists solely for the mesh's mainnet
    /// delivery role (`MeshNode::new_gateway_mainnet`, authorized by Andjroo 2026-07-06):
    /// the sender signs on their own device; this gateway only broadcasts the
    /// already-signed blob. The autonomous loop never constructs one.
    pub fn new_mainnet(rpc: Arc<dyn GatewayRpc>) -> Self {
        RpcGateway {
            rpc,
            network_id: NetworkId::Mainnet.wire_id(),
        }
    }

    /// The network-id byte this gateway broadcasts for (testnet `5`, or mainnet `24` via
    /// the Andjroo-gated [`RpcGateway::new_mainnet`]).
    pub fn network_id(&self) -> u8 {
        self.network_id
    }

    /// The shared RPC client handle (so the live example can poll inclusion with it).
    pub fn rpc(&self) -> Arc<dyn GatewayRpc> {
        self.rpc.clone()
    }
}

impl MeshGateway for RpcGateway {
    /// Direct, contextless broadcast (no validity-window check, since the bare wire carries
    /// no `validUntil`). Used when a caller has only the opaque bytes; the engine always
    /// goes through [`MeshGateway::submit_validated`] instead.
    fn submit(&self, tx_wire: Vec<u8>) -> Result<Receipt, MeshError> {
        self.submit_validated(SubmitContext {
            tx_id: mock_tx_id(&tx_wire),
            tx_wire,
            network_id: self.network_id,
            valid_until: None,
        })
    }

    fn submit_validated(&self, ctx: SubmitContext) -> Result<Receipt, MeshError> {
        // 1. networkId guard — never broadcast a tx for the wrong network.
        if ctx.network_id != self.network_id {
            return Ok(Receipt {
                tx_id: ctx.tx_id,
                status: ReceiptStatus::Failed,
            });
        }
        // 2. validity window — drop (Expired) if the head is already past validUntil. A
        //    transient head-fetch failure is propagated so no receipt is emitted.
        if let Some(valid_until) = ctx.valid_until {
            let head = self
                .rpc
                .block_number()
                .map_err(|e| MeshError::Gateway(e.to_string()))?;
            if head >= valid_until {
                return Ok(Receipt {
                    tx_id: ctx.tx_id,
                    status: ReceiptStatus::Expired,
                });
            }
        }
        // 3. broadcast. A terminal rejection (bad tx, insufficient funds) is Failed; a
        //    transient transport/overload error propagates so another gateway can retry.
        let raw_hex = bytes_to_hex(&ctx.tx_wire);
        match self.rpc.send_raw_transaction(&raw_hex) {
            Ok(_hash) => Ok(Receipt {
                tx_id: ctx.tx_id,
                status: ReceiptStatus::Accepted,
            }),
            Err(e) if e.is_transient() => Err(MeshError::Gateway(e.to_string())),
            Err(_) => Ok(Receipt {
                tx_id: ctx.tx_id,
                status: ReceiptStatus::Failed,
            }),
        }
    }

    /// G9: snapshot the live head from the RPC (`latest_block`) into a beacon on this
    /// gateway's testnet `networkId` — height **and the real block hash**, so a phone can
    /// bind an accounts proof to the beacon (`docs/BALANCE-PROOF.md`). A transient RPC
    /// failure yields `None`, so no stale beacon is emitted; an RPC client without the
    /// `latest_block` capability serves a zeroed hash, which downstream reads as honestly
    /// unbindable (`BindVerdict::BeaconUnhashed`), never as a wrong hash. Read-only.
    fn head_beacon(&self) -> Option<HeadBeacon> {
        let head = self.rpc.latest_block().ok()?;
        let mut beacon = HeadBeacon::new(head.height, self.network_id);
        beacon.block_hash = head.hash;
        Some(beacon)
    }

    /// G15: read the address's public balance via the existing read-only `get_account`, anchored
    /// to the current head (`block_number`). No NEW capability — reuses the same testnet-guarded
    /// RPC the broadcast path already uses; never broadcasts, never touches keys. A transient RPC
    /// failure or an unknown account yields `None` (the node keeps its last-known balance).
    fn balance_of(&self, address: &str) -> Option<BalanceAnswer> {
        let head_height = self.rpc.block_number().ok()?;
        let account = self.rpc.get_account(address).ok()??;
        Some(BalanceAnswer {
            balance: account.balance,
            head_height,
            network_id: self.network_id,
        })
    }

    fn history_of(&self, address: &str) -> Option<crate::tx_history::HistoryResponse> {
        use crate::nimiq::address::Address;
        use crate::nimiq::hex::hex_to_bytes;
        use crate::tx_history::{
            HistoryResponse, TxHistoryRecord, FLAG_CONFIRMED, FLAG_INCOMING, HISTORY_MAX,
        };
        let queried = Address::from_user_friendly(address).ok()?;
        let head_height = self.rpc.block_number().ok()?;
        let rows = self
            .rpc
            .get_transactions(address, HISTORY_MAX as u16)
            .ok()?;
        let compact = address.replace(' ', "").to_uppercase();
        let records = rows
            .iter()
            .take(HISTORY_MAX)
            .filter_map(|t| {
                let incoming = t.to.replace(' ', "").to_uppercase() == compact;
                let other = if incoming { &t.from } else { &t.to };
                let counterparty = Address::from_user_friendly(other).ok()?;
                let hash_bytes = hex_to_bytes(&t.hash).ok()?;
                let mut hash = [0u8; 32];
                if hash_bytes.len() != 32 {
                    return None;
                }
                hash.copy_from_slice(&hash_bytes);
                let mut flags = 0u8;
                if incoming {
                    flags |= FLAG_INCOMING;
                }
                if t.block_number.is_some() {
                    flags |= FLAG_CONFIRMED;
                }
                Some(TxHistoryRecord {
                    hash,
                    counterparty,
                    value: t.value,
                    timestamp_ms: t.timestamp_ms,
                    flags,
                })
            })
            .collect();
        Some(HistoryResponse {
            address: queried,
            head_height,
            network_id: self.network_id,
            records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_network;
    use crate::rpc::MockRpc;

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

    fn ctx(wire: &[u8], network_id: u8, valid_until: Option<u32>) -> SubmitContext {
        SubmitContext {
            tx_id: mock_tx_id(wire),
            tx_wire: wire.to_vec(),
            network_id,
            valid_until,
        }
    }

    #[test]
    fn rpc_gateway_broadcasts_and_accepts() {
        let rpc = Arc::new(MockRpc::new(1_000));
        let gw = RpcGateway::new(rpc.clone());
        assert_eq!(gw.network_id(), 5);

        let wire = vec![0xAB; 139];
        let receipt = gw
            .submit_validated(ctx(&wire, 5, Some(8_200)))
            .expect("accepted");
        assert_eq!(receipt.status, ReceiptStatus::Accepted);
        assert_eq!(receipt.tx_id, mock_tx_id(&wire));
        // Broadcast EXACTLY the hex of the opaque wire, once.
        assert_eq!(rpc.broadcasts(), vec![bytes_to_hex(&wire)]);
    }

    #[test]
    fn rpc_gateway_drops_expired_without_broadcasting() {
        // head (1000) >= validUntil (1000) -> the window has closed.
        let rpc = Arc::new(MockRpc::new(1_000));
        let gw = RpcGateway::new(rpc.clone());
        let receipt = gw
            .submit_validated(ctx(b"stale", 5, Some(1_000)))
            .expect("verdict");
        assert_eq!(receipt.status, ReceiptStatus::Expired);
        assert!(rpc.broadcasts().is_empty(), "expired tx must not broadcast");
    }

    #[test]
    fn rpc_gateway_rejects_wrong_network() {
        let rpc = Arc::new(MockRpc::new(1_000));
        let gw = RpcGateway::new(rpc.clone());
        // Mainnet networkId (24) on a testnet gateway -> Failed, never broadcast.
        let receipt = gw
            .submit_validated(ctx(b"wrong-net", 24, Some(8_200)))
            .expect("verdict");
        assert_eq!(receipt.status, ReceiptStatus::Failed);
        assert!(rpc.broadcasts().is_empty());
    }

    #[test]
    fn rpc_gateway_terminal_rejection_is_failed_receipt() {
        let rpc = Arc::new(MockRpc::new(1_000));
        rpc.reject_with("Invalid transaction");
        let gw = RpcGateway::new(rpc.clone());
        let receipt = gw
            .submit_validated(ctx(b"bad-tx", 5, Some(8_200)))
            .expect("verdict");
        assert_eq!(receipt.status, ReceiptStatus::Failed);
    }

    #[test]
    fn rpc_gateway_transient_failure_emits_no_receipt() {
        let rpc = Arc::new(MockRpc::new(1_000));
        rpc.fail_transient("connection refused");
        let gw = RpcGateway::new(rpc.clone());
        // A transient error is propagated as Err so the engine emits NO receipt.
        let err = gw.submit_validated(ctx(b"tx", 5, Some(8_200))).unwrap_err();
        assert!(matches!(err, MeshError::Gateway(_)));
    }

    #[test]
    fn rpc_gateway_no_valid_until_skips_head_check() {
        // With no validUntil, the gateway broadcasts without consulting the head.
        let rpc = Arc::new(MockRpc::new(0));
        let gw = RpcGateway::new(rpc.clone());
        let receipt = gw.submit(b"no-window".to_vec()).expect("accepted");
        assert_eq!(receipt.status, ReceiptStatus::Accepted);
        assert_eq!(rpc.broadcasts().len(), 1);
    }
}
