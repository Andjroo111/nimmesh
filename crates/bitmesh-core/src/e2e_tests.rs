//! # e2e_tests — headless end-to-end pay loop + the four ADR-0002 callback gotchas
//!
//! The decisive testability win of ADR-0002: with [`BleRadio`] a plain trait, the whole
//! GOAL.md demo loop (`submit → real-packet flood → TTL relay → dedup → gateway →
//! receipt → settled`) runs deterministically under `cargo test`, no phone. These tests
//! drive crate-private observability hooks, so they live inside the crate (not `tests/`).
//!
//! Coverage:
//! - the **full pay loop** with real G4 packets through a (mock) gateway;
//! - gotcha **(a)** `on_packet_received` never re-enters `radio.send` synchronously;
//! - gotcha **(b)** `send` is fire-and-forget, outcomes arrive via `on_send_result`;
//! - gotcha **(c)** a panicking handler can't abort the worker (hot path infallible);
//! - gotcha **(d)** teardown releases the radio with no leaked node (weak-edge discipline);
//! - the harness knobs: **latency / loss / partition**.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use crate::codec::encode;
use crate::default_network;
use crate::engine::PaymentStatus;
use crate::envelope::{encode_envelope, NimiqEnvelope};
use crate::gateway::{MeshGateway, MockGateway, Receipt, ReceiptStatus};
use crate::mock_radio::{MeshHarness, MockEther, MockRadio};
use crate::node::MeshNode;
use crate::packet::{MessageType, Packet, BROADCAST_RECIPIENT};
use crate::radio::BleRadio;
use crate::transport::{mock_tx_id, MeshError, TxId};

const SETTLE: Duration = Duration::from_secs(3);

/// Poll `f` until it is true or `timeout` elapses; returns its final value.
fn wait_until<F: Fn() -> bool>(f: F, timeout: Duration) -> bool {
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
fn make_tx_packet(sender: [u8; 8], tx_wire: &[u8], ttl: u8, ts: u64) -> Vec<u8> {
    let mut env = NimiqEnvelope::new(tx_wire.to_vec());
    env.tx_id = Some(mock_tx_id(tx_wire).0);
    let payload = encode_envelope(&env).expect("envelope encodes");
    let mut p = Packet::new(MessageType::NimiqTx, sender, payload);
    p.recipient_id = Some(BROADCAST_RECIPIENT);
    p.ttl = ttl;
    p.timestamp_ms = ts;
    encode(&p).expect("packet encodes")
}

/// A real `nimiqTxReceipt` (0x31) wire frame: payload is `txId(32) | status(1)`.
fn make_receipt_packet(
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

// --- The headline end-to-end test ------------------------------------------------

#[test]
fn full_offline_pay_loop_settles_with_real_packets() {
    // origin <-> relay <-> gateway: the origin is NOT directly linked to the gateway, so
    // the payment MUST traverse the relay at TTL=7 (real bitmesh packets the whole way).
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    // G3: an opaque stand-in for the real signed Nimiq tx wire (airplane mode).
    let opaque = b"opaque-signed-tx-bytes-from-airplane-mode".to_vec();
    let tx_id = origin.submit_local_tx(opaque.clone());

    // The receipt floods back through the relay and settles the origin.
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    // The gateway "broadcast" exactly the opaque bytes the origin signed, once.
    assert_eq!(gw.submission_count(), 1);
    assert_eq!(gw.submissions()[0], opaque);
    // The blind relay actually carried traffic (tx forward + receipt forward).
    assert!(relay.forwarded_count() >= 1);
    // The id the origin tracks is the mock txId of the opaque bytes.
    assert_eq!(tx_id, mock_tx_id(&opaque).0.to_vec());

    h.shutdown();
}

#[test]
fn rejected_tx_surfaces_failed_not_settled() {
    // Core value #5: never show "paid" without inclusion; a rejection is explicit.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    gw.force_status(ReceiptStatus::Expired);
    let origin = h.add_node("origin", &[1]);
    h.add_node("relay", &[2]);
    h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let tx_id = origin.submit_local_tx(b"stale-tx".to_vec());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Failed);
    assert_eq!(gw.submission_count(), 1);
    h.shutdown();
}

#[test]
fn gateway_submits_once_despite_two_relay_paths() {
    // Diamond: origin reaches the gateway via two independent relay paths; LRU dedup
    // (blind at the relays, txId-keyed at the gateway) yields exactly one submission.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    h.add_node("r1", &[2]);
    h.add_node("r2", &[3]);
    h.add_gateway("gw", &[4], gw.clone());
    h.connect("origin", "r1");
    h.connect("origin", "r2");
    h.connect("r1", "gw");
    h.connect("r2", "gw");

    let tx_id = origin.submit_local_tx(b"tx".to_vec());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    assert_eq!(gw.submission_count(), 1);
    h.shutdown();
}

// --- Gotcha (a): non-blocking on_packet_received -------------------------------

/// A `BleRadio` that records every `send` with the thread it ran on — used to prove the
/// relay's `radio.send` never happens synchronously inside `on_packet_received`.
struct SpyRadio {
    sends: Mutex<Vec<(String, ThreadId)>>,
    stopped: AtomicBool,
}

impl SpyRadio {
    fn new() -> Arc<Self> {
        Arc::new(SpyRadio {
            sends: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        })
    }
    fn send_count(&self) -> usize {
        self.sends.lock().unwrap().len()
    }
    fn send_threads(&self) -> Vec<ThreadId> {
        self.sends.lock().unwrap().iter().map(|(_, t)| *t).collect()
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

#[test]
fn on_packet_received_never_re_enters_radio_send_synchronously() {
    // ADR-0002 gotcha (a): on iOS a synchronous `radio.send` from inside
    // `on_packet_received` re-enters CoreBluetooth's dispatch queue. So the relay's send
    // MUST happen on the worker thread, never on the callback thread.
    let test_thread = std::thread::current().id();
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new(vec![9], radio);
    node.on_peer_connected("x".to_string());

    // A foreign nimiqTx the node will relay (TTL 7 > 1, sender != ours).
    let bytes = make_tx_packet([1; 8], b"opaque", 7, 1234);
    node.on_packet_received(bytes);

    // The relay eventually fires `radio.send` — on the worker thread.
    assert!(
        wait_until(|| spy.send_count() >= 1, Duration::from_secs(1)),
        "relay never sent"
    );
    for tid in spy.send_threads() {
        assert_ne!(
            tid, test_thread,
            "radio.send re-entered synchronously on the callback thread"
        );
    }
    node.shutdown();
}

// --- Gotcha (b): fire-and-forget send, outcomes via on_send_result -------------

#[test]
fn send_is_fire_and_forget_with_delivery_outcomes() {
    let mut h = MeshHarness::new();
    let a = h.add_node("a", &[1]);
    h.add_node("b", &[2]);
    h.connect("a", "b");

    // A floods a local tx to B (no gateway → B just relays/drops); the delivery succeeds
    // and the async outcome comes back to A via on_send_result.
    a.submit_local_tx(b"tx".to_vec());
    assert!(
        wait_until(|| a.send_ok() >= 1, Duration::from_secs(1)),
        "no successful send outcome reported"
    );
    assert!(a.send_attempts() >= 1);
    assert_eq!(a.send_fail(), 0);
    h.shutdown();
}

#[test]
fn total_loss_reports_failed_outcomes_and_stays_pending() {
    let mut h = MeshHarness::new();
    h.ether().set_loss(1.0); // every hop drops.
    let origin = h.add_node("origin", &[1]);
    h.add_node("relay", &[2]);
    let gw = Arc::new(MockGateway::new(default_network()));
    h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let tx_id = origin.submit_local_tx(b"tx".to_vec());
    // Nothing is delivered, so the origin honestly stays Pending …
    assert_eq!(
        origin.wait_payment(&tx_id, Duration::from_millis(300)),
        PaymentStatus::Pending
    );
    // … and the failed write outcome is reported (fire-and-forget result path).
    assert!(wait_until(
        || origin.send_fail() >= 1,
        Duration::from_secs(1)
    ));
    assert_eq!(gw.submission_count(), 0);
    h.shutdown();
}

// --- Gotcha (c): a panicking handler can't abort the worker --------------------

/// A gateway whose `submit` panics — models a hostile/buggy handler throwing mid-flood.
struct PanicGateway;
impl MeshGateway for PanicGateway {
    fn submit(&self, _tx_wire: Vec<u8>) -> Result<Receipt, MeshError> {
        panic!("simulated handler explosion during a flood burst");
    }
}

#[test]
fn worker_survives_a_panicking_handler() {
    // ADR-0002 gotcha (c): an unguarded throw during a flood burst must not abort the
    // process. The worker wraps each job in catch_unwind, so it keeps draining.
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new_gateway(vec![7], radio, Arc::new(PanicGateway));
    node.on_peer_connected("x".to_string());

    // First: a tx that drives the gateway to panic (caught; no send for this one).
    node.on_packet_received(make_tx_packet([1; 8], b"boom", 7, 100));
    std::thread::sleep(Duration::from_millis(20));

    // Then: a receipt the worker must still process and relay — proving it is alive.
    let receipt = make_receipt_packet([2; 8], mock_tx_id(b"boom"), ReceiptStatus::Accepted, 7, 200);
    node.on_packet_received(receipt);
    assert!(
        wait_until(|| spy.send_count() >= 1, Duration::from_secs(1)),
        "worker died after the panic"
    );
    node.shutdown();
}

// --- Gotcha (d): teardown releases the radio, no leaked node -------------------

#[test]
fn teardown_releases_radio_and_leaks_no_node() {
    // ADR-0002 gotcha (d): the node holds the radio strongly; the radio holds the node
    // weakly. Tearing the node down must stop the radio and free the node with no leak.
    let ether = MockEther::new();
    let radio = MockRadio::new("n", ether.clone());
    let node = MeshNode::new(vec![1], radio.clone());
    radio.bind(Arc::downgrade(&node));

    let weak = Arc::downgrade(&node);
    node.shutdown();
    assert!(radio.is_stopped(), "shutdown did not release the radio");

    // Drop the only strong handle: the worker holds none, the radio/ether hold weak refs,
    // so the node must be reclaimed.
    drop(node);
    assert!(
        weak.upgrade().is_none(),
        "node leaked — a strong cross-reference pins it"
    );

    ether.shutdown();
}

// --- Harness knobs: latency / partition ----------------------------------------

#[test]
fn latency_delays_but_still_settles() {
    let mut h = MeshHarness::new();
    h.ether().set_latency(Duration::from_millis(3));
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    h.add_node("relay", &[2]);
    h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let tx_id = origin.submit_local_tx(b"slow-but-arrives".to_vec());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    h.shutdown();
}

#[test]
fn partition_isolates_the_gateway_and_keeps_pending() {
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    h.add_node("relay", &[2]);
    h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");
    // Cut the only path to the gateway.
    h.ether().partition("relay", "gw");

    let tx_id = origin.submit_local_tx(b"into-the-void".to_vec());
    assert_eq!(
        origin.wait_payment(&tx_id, Duration::from_millis(300)),
        PaymentStatus::Pending
    );
    assert_eq!(gw.submission_count(), 0);

    // Healing the partition lets a fresh submission settle.
    h.ether().heal("relay", "gw");
    let tx2 = origin.submit_local_tx(b"now-it-flows".to_vec());
    assert_eq!(origin.wait_payment(&tx2, SETTLE), PaymentStatus::Settled);
    h.shutdown();
}
