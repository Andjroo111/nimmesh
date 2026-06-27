//! # test_support — shared headless-test scaffolding (`cfg(test)` only)
//!
//! The wire-frame builders + the spy radio + the poll helper the in-crate end-to-end
//! tests share ([`crate::e2e_tests`] for the G5–G8 pay loop, [`crate::beacon_e2e_tests`]
//! for the G9 head-beacon / validity-window guard). Kept in one place so the two suites
//! stay DRY and every test file stays well under the 800-line ceiling. **Test-only** — none
//! of this is compiled into a shipping build.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crate::beacon::{encode_beacon, HeadBeacon};
use crate::codec::encode;
use crate::envelope::{encode_envelope, NimiqEnvelope};
use crate::gateway::ReceiptStatus;
use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
use crate::radio::BleRadio;
use crate::transport::{mock_tx_id, TxId};

/// The standard settle/poll budget for the headless tests.
pub(crate) const SETTLE: Duration = Duration::from_secs(3);

/// Poll `f` until it is true or `timeout` elapses; returns its final value.
pub(crate) fn wait_until<F: Fn() -> bool>(f: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    f()
}

/// A real `nimiqTx` (0x30) wire frame carrying an opaque `tx_wire` inside the TLV
/// envelope — exactly what an origin floods.
pub(crate) fn make_tx_packet(sender: [u8; 8], tx_wire: &[u8], ttl: u8, ts: u64) -> Vec<u8> {
    let mut env = NimiqEnvelope::new(tx_wire.to_vec());
    env.tx_id = Some(mock_tx_id(tx_wire).0);
    let payload = encode_envelope(&env).expect("envelope encodes");
    let mut p = Packet::new(MessageType::NimiqTx, sender, payload);
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// Like [`make_tx_packet`] but stamps a `validUntil` height in the TLV envelope, so the
/// validity-window check (head vs `validUntil`) is exercised end to end.
pub(crate) fn make_tx_packet_valid_until(
    sender: [u8; 8],
    tx_wire: &[u8],
    valid_until: u32,
    ttl: u8,
    ts: u64,
) -> Vec<u8> {
    let mut env = NimiqEnvelope::new(tx_wire.to_vec());
    env.tx_id = Some(mock_tx_id(tx_wire).0);
    env.valid_until = Some(valid_until);
    let payload = encode_envelope(&env).expect("envelope encodes");
    let mut p = Packet::new(MessageType::NimiqTx, sender, payload);
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// A real `nimiqTxReceipt` (0x31) wire frame: payload is `txId(32) | status(1)`.
pub(crate) fn make_receipt_packet(
    sender: [u8; 8],
    tx_id: TxId,
    status: ReceiptStatus,
    ttl: u8,
    ts: u64,
) -> Vec<u8> {
    let mut payload = tx_id.0.to_vec();
    payload.push(status.code());
    let mut p = Packet::new(MessageType::NimiqTxReceipt, sender, payload);
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// A real `fragment` (0x20) wire frame carrying one fragment payload.
pub(crate) fn make_fragment_packet(
    sender: [u8; 8],
    frag_payload: Vec<u8>,
    ttl: u8,
    ts: u64,
) -> Vec<u8> {
    let mut p = Packet::new(MessageType::Fragment, sender, frag_payload);
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// A real `nimiqHeadBeacon` (0x32) wire frame carrying `{height | blockHash | networkId}`.
pub(crate) fn make_beacon_packet(
    sender: [u8; 8],
    height: u32,
    network_id: u8,
    ttl: u8,
    ts: u64,
) -> Vec<u8> {
    let beacon = HeadBeacon::new(height, network_id);
    let mut p = Packet::new(MessageType::NimiqHeadBeacon, sender, encode_beacon(&beacon));
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// A `BleRadio` that records every `send` with the thread it ran on — used to prove the
/// relay's `radio.send` never happens synchronously inside `on_packet_received`, and to
/// assert exactly which peers a packet was (or was not) relayed to.
pub(crate) struct SpyRadio {
    sends: Mutex<Vec<(String, ThreadId)>>,
    stopped: AtomicBool,
}

impl SpyRadio {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(SpyRadio {
            sends: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        })
    }
    pub(crate) fn send_count(&self) -> usize {
        self.sends.lock().unwrap().len()
    }
    pub(crate) fn send_threads(&self) -> Vec<ThreadId> {
        self.sends.lock().unwrap().iter().map(|(_, t)| *t).collect()
    }
    pub(crate) fn send_peers(&self) -> Vec<String> {
        self.sends
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

impl BleRadio for SpyRadio {
    fn start_advertising(&self) {}
    fn start_scanning(&self) {}
    fn send(&self, peer_id: String, _bytes: Vec<u8>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        self.sends
            .lock()
            .unwrap()
            .push((peer_id, std::thread::current().id()));
    }
    fn disconnect(&self, _peer_id: String) {}
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}
