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

use std::sync::Arc;
use std::time::Duration;

use crate::citizen::RelayPosture;
use crate::default_network;
use crate::fragment::fragment_message;
use crate::gateway::{MeshGateway, MockGateway, Receipt, ReceiptStatus, RpcGateway};
use crate::mock_radio::{MeshHarness, MockEther, MockRadio};
use crate::nimiq::address::Address;
use crate::node::MeshNode;
use crate::packet::MessageType;
use crate::radio::BleRadio;
use crate::relay::RelayPolicy;
use crate::rpc::MockRpc;
use crate::settlement::{PaymentStatus, SettlementDirection};
use crate::test_support::{
    make_fragment_packet, make_receipt_packet, make_tx_packet, make_tx_packet_valid_until,
    wait_until, SpyRadio, SETTLE,
};
use crate::transport::{mock_tx_id, MeshError};

// --- The headline end-to-end test ------------------------------------------------

#[test]
fn full_offline_pay_loop_settles_with_real_packets() {
    // origin <-> relay <-> gateway: the origin is NOT directly linked to the gateway, so
    // the payment MUST traverse the relay at TTL=7 (real nimmesh packets the whole way).
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

// --- G15: balance over the mesh ---------------------------------------------------

#[test]
fn balance_query_is_answered_by_a_gateway_and_cached_across_the_mesh() {
    // origin <-> relay <-> gateway: the origin floods a balance query at TTL=7; only the
    // gateway (internet) can answer; its `nimiqBalanceResponse` floods back through the relay
    // and the origin caches the (unverified, last-known) balance + the head it was read at.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    gw.set_balance(11_000_000_000, 4_428_402); // 110_000 NIM at testnet head 4_428_402
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_node("relay", &[2]);
    let gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    // A valid address that round-trips through the user-friendly <-> 20-byte codec.
    let addr = Address::from_bytes([0x11; 20]).to_user_friendly();
    origin.query_balance(addr.clone());

    assert!(
        wait_until(|| origin.test_cached_balance(&addr).is_some(), SETTLE),
        "the balance response never reached the origin"
    );
    let cached = origin.test_cached_balance(&addr).expect("balance cached");
    assert_eq!(cached.balance, 11_000_000_000);
    assert_eq!(cached.head_height, 4_428_402);
    assert_eq!(cached.network_id, default_network().wire_id());
    // The gateway answered exactly once (the query dedups across relay echoes).
    assert_eq!(gateway.balance_answered(), 1);
    // The blind relay carried the query + the response.
    assert!(relay.forwarded_count() >= 1);
    h.shutdown();
}

#[test]
fn gateway_with_no_balance_answers_nothing() {
    // A gateway that has no chain view (no `set_balance`) answers nothing — the node simply
    // never learns a balance, rather than caching a wrong/zero one.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network())); // no balance configured
    let origin = h.add_node("origin", &[1]);
    let gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "gw");

    let addr = Address::from_bytes([0x22; 20]).to_user_friendly();
    origin.query_balance(addr.clone());

    // Give the query time to flood + (not) be answered.
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(gateway.balance_answered(), 0);
    assert!(origin.test_cached_balance(&addr).is_none());
    h.shutdown();
}

// --- G17: settlement closure for both parties ----------------------------------

#[test]
fn the_same_receipt_settles_both_sender_and_receiver() {
    // origin <-> gw <-> receiver. The origin sends; the gateway accepts and floods ONE
    // `nimiqTxReceipt`. It closes the sender's "did it land?" (Outgoing) AND the receiver's
    // "did I get paid?" (Incoming) — the receiver registered the txId via `watch_incoming`.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let _gateway = h.add_gateway("gw", &[2], gw.clone());
    let receiver = h.add_node("receiver", &[3]);
    h.connect("origin", "gw");
    h.connect("gw", "receiver");

    let opaque = b"opaque-signed-tx".to_vec();
    // The payee learns the txId out of band (the request / confirmation flow) and watches it.
    let tx_id = mock_tx_id(&opaque).0.to_vec();
    receiver.watch_incoming(tx_id.clone());

    let sent_id = origin.submit_local_tx(opaque);
    assert_eq!(sent_id, tx_id);

    // Sender side closes.
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    // Receiver side closes from the very same flooded receipt.
    assert_eq!(
        receiver.wait_payment(&tx_id, SETTLE),
        PaymentStatus::Settled
    );
    let s = receiver.settlement(tx_id).expect("receiver tracks it");
    assert_eq!(s.status, PaymentStatus::Settled);
    assert_eq!(s.direction, SettlementDirection::Incoming);

    h.shutdown();
}

#[test]
fn a_reject_receipt_fails_the_receiver_too() {
    // A NACK (expired/failed) closes the receiver's view as Failed, not a false "received".
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    gw.force_status(ReceiptStatus::Expired);
    let origin = h.add_node("origin", &[1]);
    let _gateway = h.add_gateway("gw", &[2], gw.clone());
    let receiver = h.add_node("receiver", &[3]);
    h.connect("origin", "gw");
    h.connect("gw", "receiver");

    let opaque = b"stale".to_vec();
    let tx_id = mock_tx_id(&opaque).0.to_vec();
    receiver.watch_incoming(tx_id.clone());
    origin.submit_local_tx(opaque);

    assert_eq!(receiver.wait_payment(&tx_id, SETTLE), PaymentStatus::Failed);
    h.shutdown();
}

// G12 hardening (verify-before-relay / rate-limit / stop-after-ACK) e2e tests live in
// `hardening_e2e_tests.rs` (kept separate to stay under the 800-line file guard).

// --- G20: good-citizen + battery-aware relay -----------------------------------

#[test]
fn a_critical_battery_node_relays_nothing_for_others() {
    // origin <-> relay <-> gw; the relay is on a critical battery (5%, unplugged) → posture
    // Off. Being a good citizen must never cost the user, so it carries nothing for others:
    // the payment can't reach the gateway and stays Pending (honest, unconfirmed-until-inclusion).
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    relay.set_battery(5, false); // 5% on battery → RelayPosture::Off

    let tx_id = origin.submit_local_tx(b"tx".to_vec());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Pending);
    assert_eq!(gw.submission_count(), 0);
    assert_eq!(relay.forwarded_count(), 0);
    assert_eq!(relay.relay_stats().payments_relayed, 0);
    assert!(matches!(relay.relay_stats().posture, RelayPosture::Off));

    h.shutdown();
}

#[test]
fn a_charging_node_relays_and_counts_helped_payments() {
    // Same line, but the relay is plugged in (15% but charging) → posture Full. It carries
    // the payment to the gateway, settles the origin, and counts itself a good citizen.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    relay.set_battery(15, true); // low %, but charging → full participation

    let tx_id = origin.submit_local_tx(b"tx".to_vec());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    let stats = relay.relay_stats();
    assert!(
        stats.payments_relayed >= 1,
        "the carried payment should be counted"
    );
    assert!(stats.packets_relayed >= 1);
    assert!(matches!(stats.posture, RelayPosture::Full));

    h.shutdown();
}

// --- Gotcha (a): non-blocking on_packet_received -------------------------------

#[test]
fn on_packet_received_never_re_enters_radio_send_synchronously() {
    // ADR-0002 gotcha (a): on iOS a synchronous `radio.send` from inside
    // `on_packet_received` re-enters CoreBluetooth's dispatch queue. So the relay's send
    // MUST happen on the worker thread, never on the callback thread.
    let test_thread = std::thread::current().id();
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new_with_policy(vec![9], radio, RelayPolicy::deterministic());
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
    let node = MeshNode::new_gateway_with_policy(
        vec![7],
        radio,
        Arc::new(PanicGateway),
        RelayPolicy::deterministic(),
    );
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
    let node = MeshNode::new_with_policy(vec![1], radio.clone(), RelayPolicy::deterministic());
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

// --- G8: the real RpcGateway over a MockRpc (offline, no network) ----------------

#[test]
fn full_pay_loop_settles_through_rpc_gateway() {
    // The real G8 gateway (RpcGateway) wired into the mesh, backed by an offline MockRpc.
    // origin <-> relay <-> gateway: the receipt of a real broadcast settles the origin,
    // and the gateway broadcast EXACTLY the opaque bytes the origin flooded — once.
    let mut h = MeshHarness::new();
    let rpc = Arc::new(MockRpc::new(1_000));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new(rpc.clone()));
    let origin = h.add_node("origin", &[1]);
    h.add_node("relay", &[2]);
    h.add_gateway("gw", &[3], gw);
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let opaque = b"signed-testnet-tx-bytes".to_vec();
    let tx_id = origin.submit_local_tx(opaque.clone());
    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    // The gateway broadcast exactly the hex of the opaque wire, exactly once.
    assert_eq!(
        rpc.broadcasts(),
        vec![crate::nimiq::hex::bytes_to_hex(&opaque)]
    );
    h.shutdown();
}

#[test]
fn rpc_gateway_never_broadcasts_an_expired_tx() {
    // The money-path safety property: a tx whose validity window has already closed
    // (head >= validUntil) is DROPPED — the gateway never puts it on chain. Feed a gateway
    // node a nimiqTx stamped validUntil=500 while the node's RPC head is 1000.
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let rpc = Arc::new(MockRpc::new(1_000));
    let gw: Arc<dyn MeshGateway> = Arc::new(RpcGateway::new(rpc.clone()));
    let node = MeshNode::new_gateway_with_policy(vec![3], radio, gw, RelayPolicy::deterministic());
    node.on_peer_connected("peer".to_string());

    node.on_packet_received(make_tx_packet_valid_until([1; 8], b"stale", 500, 7, 100));
    // The gateway emits an (Expired) receipt back into the mesh …
    assert!(
        wait_until(|| spy.send_count() >= 1, Duration::from_secs(1)),
        "gateway never emitted an expired receipt"
    );
    // … but it NEVER broadcast the expired tx to the chain.
    assert!(
        rpc.broadcasts().is_empty(),
        "an expired tx must never reach sendRawTransaction"
    );
    node.shutdown();
}

// --- G6: source-link exclusion --------------------------------------------------

#[test]
fn relay_never_echoes_back_out_the_source_link() {
    // PROTOCOL.md: "the source link is excluded." A node with two peers that receives a
    // relayable packet *from* one of them must rebroadcast only to the *other* — never
    // back out the link it arrived on.
    let spy = SpyRadio::new();
    let radio: Arc<dyn BleRadio> = spy.clone();
    let node = MeshNode::new_with_policy(vec![9], radio, RelayPolicy::deterministic());
    node.on_peer_connected("src".to_string());
    node.on_peer_connected("other".to_string());

    // A foreign nimiqTx arriving from "src" (TTL 7 > 1, degree 2 < threshold → relays).
    let bytes = make_tx_packet([1; 8], b"opaque", 7, 7777);
    node.on_packet_received_from("src".to_string(), bytes);

    assert!(
        wait_until(|| spy.send_count() >= 1, Duration::from_secs(1)),
        "relay never forwarded"
    );
    // Give any (erroneous) second write a moment to surface, then assert exclusion.
    std::thread::sleep(Duration::from_millis(20));
    let peers = spy.send_peers();
    assert!(
        peers.contains(&"other".to_string()),
        "did not relay to the non-source peer"
    );
    assert!(
        !peers.contains(&"src".to_string()),
        "relay echoed back out the source link"
    );
    node.shutdown();
}

// --- G6: fragmentation + reassembly through the live engine ---------------------

#[test]
fn fragmented_receipt_reassembles_and_settles_the_origin() {
    // A receipt larger than one (hypothetical) BLE chunk arrives as several `fragment`
    // (0x20) packets. The origin's reassembler must rebuild it, dispatch it locally
    // (TTL zeroed → no re-flood), and settle the pending payment — exercising the whole
    // fragment → reassemble → dispatch path through the engine, not just the unit module.
    let mut h = MeshHarness::new();
    let origin = h.add_node("origin", &[1]);
    // Originate a payment so the origin has a Pending slot keyed by mock_tx_id(opaque).
    let opaque = b"opaque-tx-awaiting-a-fragmented-receipt".to_vec();
    let tx_id = origin.submit_local_tx(opaque.clone());
    assert_eq!(
        origin.wait_payment(&tx_id, Duration::from_millis(100)),
        PaymentStatus::Pending
    );

    // Build the receipt payload (txId(32) | status(1)) and fragment it at a tiny 20-B
    // chunk so it genuinely splits into multiple fragments.
    let mut receipt_payload = mock_tx_id(&opaque).0.to_vec();
    receipt_payload.push(ReceiptStatus::Accepted.code());
    let frags = fragment_message(
        MessageType::NimiqTxReceipt.to_u8(),
        &receipt_payload,
        20,
        [42; 8],
    );
    assert!(
        frags.len() > 1,
        "a 33-B receipt at 20-B chunks must fragment"
    );

    // Deliver every fragment packet to the origin (distinct timestamps → distinct keys).
    for (i, fp) in frags.into_iter().enumerate() {
        let bytes = make_fragment_packet([2; 8], fp, 7, 9000 + i as u64);
        origin.on_packet_received(bytes);
    }

    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    h.shutdown();
}

// --- G7: store-and-forward via GCS gossip-sync ----------------------------------

#[test]
fn offline_node_catches_up_via_gcs_gossip_sync() {
    // PROTOCOL.md "Store-and-forward = GCS gossip-sync": node A floods several packets
    // while node B is **out of range** (not yet linked), so B hears none of them. When B
    // rejoins it issues a `requestSync` advertising its (empty) GCS "have" filter; A
    // unicasts back exactly the packets B is missing, flagged `isRSR`. B must end up with
    // the full set and no duplicates — all inside the 15-min retention window.
    let mut h = MeshHarness::new();
    let a = h.add_node("a", &[1]);
    let b = h.add_node("b", &[2]);

    // A originates N packets while B is offline (unlinked → A sends to no one, but caches
    // each in its store-and-forward cache).
    let n = 12usize;
    for i in 0..n {
        a.submit_local_tx(format!("offline-tx-{i}").into_bytes());
    }
    assert!(
        wait_until(|| a.recent_stored() >= n, Duration::from_secs(1)),
        "A never cached its own floods"
    );
    assert_eq!(
        b.recent_stored(),
        0,
        "B should have heard nothing while offline"
    );

    // B rejoins the mesh and asks for a catch-up.
    h.connect("a", "b");
    b.request_sync();

    // A serves every missing packet; B absorbs them all, exactly once.
    assert!(
        wait_until(|| b.recent_stored() >= n, SETTLE),
        "B did not catch up via gossip-sync"
    );
    assert_eq!(b.recent_stored(), n, "B caught up with the wrong count");
    assert_eq!(
        b.rsr_received(),
        n,
        "B received the wrong number of isRSR replies"
    );
    assert_eq!(
        a.rsr_sent(),
        n,
        "A replied with the wrong number of missing packets"
    );

    // Idempotent: B now advertises the full set, so a second sync pulls nothing new and
    // leaks no duplicates into B's cache.
    b.request_sync();
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(b.recent_stored(), n, "a duplicate leaked into B's cache");
    assert_eq!(a.rsr_sent(), n, "A re-sent packets B already had");

    h.shutdown();
}

#[test]
fn gossip_sync_only_sends_the_packets_a_peer_lacks() {
    // B already has SOME of A's packets (it was online for the first few, then dropped).
    // On rejoin, A's reply must cover only the gap — not the packets B already holds.
    let mut h = MeshHarness::new();
    let a = h.add_node("a", &[1]);
    let b = h.add_node("b", &[2]);
    h.connect("a", "b");

    // First few packets arrive while B is connected (B caches them via the relay path).
    let early = 4usize;
    for i in 0..early {
        a.submit_local_tx(format!("early-{i}").into_bytes());
    }
    assert!(
        wait_until(|| b.recent_stored() >= early, Duration::from_secs(1)),
        "B never heard the early packets"
    );

    // B drops offline; A floods more that B misses.
    h.ether().partition("a", "b");
    let late = 7usize;
    for i in 0..late {
        a.submit_local_tx(format!("late-{i}").into_bytes());
    }
    assert!(wait_until(
        || a.recent_stored() >= early + late,
        Duration::from_secs(1)
    ));

    // B rejoins and syncs: A should send back only the `late` packets B is missing.
    h.ether().heal("a", "b");
    b.request_sync();
    assert!(
        wait_until(|| b.recent_stored() >= early + late, SETTLE),
        "B did not fill the gap"
    );
    assert_eq!(b.recent_stored(), early + late);
    assert_eq!(
        a.rsr_sent(),
        late,
        "A re-sent packets B already had instead of only the gap"
    );

    h.shutdown();
}

#[test]
fn maintenance_poll_is_rate_limited_to_the_tick() {
    // The 30 s maintenance tick must fire its first poll but rate-limit the rest, so a
    // burst of polls issues at most one `requestSync` (which a peer answers once).
    let mut h = MeshHarness::new();
    let a = h.add_node("a", &[1]);
    let b = h.add_node("b", &[2]);

    // A originates one packet while B is offline (unlinked), so the ONLY way B can learn
    // of it is via gossip-sync — not the normal flood.
    a.submit_local_tx(b"only-tx".to_vec());
    assert!(wait_until(
        || a.recent_stored() >= 1,
        Duration::from_secs(1)
    ));

    // B rejoins, then hammers its maintenance poll; only the first is due within one tick.
    h.connect("a", "b");
    for _ in 0..10 {
        b.poll_sync();
    }
    assert!(
        wait_until(|| b.recent_stored() >= 1, SETTLE),
        "the first maintenance poll never synced B"
    );
    std::thread::sleep(Duration::from_millis(40));
    // A answered exactly one sync round (one missing packet), not ten.
    assert_eq!(
        a.rsr_sent(),
        1,
        "maintenance poll was not rate-limited to the tick"
    );
    assert_eq!(b.recent_stored(), 1);

    h.shutdown();
}
