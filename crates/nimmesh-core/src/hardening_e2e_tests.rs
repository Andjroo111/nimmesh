//! # hardening_e2e_tests — G12 anti-spam / anti-DoS end-to-end (`cfg(test)` only)
//!
//! The headless mesh proofs for the G12 hardening layer (RISKS.md #4): verify-before-relay
//! (a free spam filter), per-peer rate limiting (anti-DoS), and stop-after-ACK (don't
//! re-carry a landed tx). Split out of [`crate::e2e_tests`] to keep each test file under the
//! 800-line guard (mirrors [`crate::beacon_e2e_tests`]). Shares the wire-frame builders +
//! spy radio in [`crate::test_support`].

use std::sync::Arc;

use crate::default_network;
use crate::gateway::{MockGateway, ReceiptStatus};
use crate::mock_radio::MeshHarness;
use crate::nimiq::address::Address;
use crate::node::MeshNode;
use crate::relay::RelayPolicy;
use crate::settlement::PaymentStatus;
use crate::test_support::{make_receipt_packet, make_tx_packet, wait_until, SpyRadio, SETTLE};
use crate::transport::mock_tx_id;

#[test]
fn a_verifying_relay_drops_junk_but_carries_a_real_signed_tx() {
    use crate::nimiq::Transfer;
    use ed25519_dalek::{Signer, SigningKey};

    // origin -> verifying relay -> gateway. The relay runs the G12 spam filter.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_verifying_node("relay", &[2]); // verify-before-relay ON
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    // Junk that claims to be a tx → the verifying relay drops it; the gateway never sees it.
    let junk_id = origin.submit_local_tx(b"junk-not-a-signed-tx".to_vec());
    assert_eq!(
        origin.wait_payment(&junk_id, SETTLE),
        PaymentStatus::Pending
    );
    assert_eq!(gw.submission_count(), 0);
    assert!(
        relay.verify_dropped() >= 1,
        "the relay should have dropped the junk"
    );

    // A real signed transfer → verifies, relays, settles, and is the only thing carried.
    let sk = SigningKey::from_bytes(&[9u8; 32]);
    let pubkey = sk.verifying_key().to_bytes();
    let t = Transfer {
        sender: Address::from_public_key(&pubkey),
        recipient: Address::from_bytes([0x22; 20]),
        value: 250_000,
        fee: 0,
        validity_start_height: 100,
        network_id: default_network().wire_id(),
    };
    let sig = sk.sign(&t.serialize_content()).to_bytes();
    let wire = t.serialize_basic(&pubkey, &sig);
    let tx_id = origin.submit_local_tx(wire.clone());

    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    assert_eq!(gw.submission_count(), 1); // only the real tx made it across
    assert_eq!(gw.submissions()[0], wire);

    h.shutdown();
}

#[test]
fn a_flooding_peer_is_rate_limited() {
    // G12 per-peer rate limit: a peer that floods us far beyond its bucket gets throttled —
    // the excess frames are dropped before any decode/relay airtime is spent on them.
    let radio = SpyRadio::new();
    let node = MeshNode::new_with_policy(vec![6], radio.clone(), RelayPolicy::deterministic());
    node.on_peer_connected("flood".to_string());

    let frame = make_tx_packet([7; 8], b"spam", 7, 1);
    for _ in 0..1000 {
        node.on_packet_received_from("flood".to_string(), frame.clone());
    }

    // The bucket (256) is far smaller than 1000 frames arriving faster than it refills, so
    // many are dropped by the limiter.
    assert!(
        wait_until(|| node.rate_limited() >= 1, SETTLE),
        "a flooding peer should be rate-limited"
    );
    node.shutdown();
}

#[test]
fn a_node_stops_carrying_a_tx_once_its_receipt_is_seen() {
    // G12 stop-after-ACK: a node that has seen a gateway receipt for a txId must not keep
    // re-carrying copies of that (already-landed) tx — it just wastes mesh airtime.
    let radio = SpyRadio::new();
    let node = MeshNode::new_with_policy(vec![5], radio.clone(), RelayPolicy::deterministic());
    node.on_peer_connected("a".to_string());
    node.on_peer_connected("b".to_string());

    let wire = b"already-landed-tx".to_vec();
    let tx_id = mock_tx_id(&wire);
    // 1) the gateway receipt (ACK) for this txId arrives …
    node.on_packet_received_from(
        "a".to_string(),
        make_receipt_packet([9; 8], tx_id, ReceiptStatus::Accepted, 7, 1),
    );
    // 2) … then a fresh copy of the tx itself arrives → it must be dropped, not re-flooded.
    node.on_packet_received_from("a".to_string(), make_tx_packet([8; 8], &wire, 7, 2));

    assert!(
        wait_until(|| node.stop_after_ack() >= 1, SETTLE),
        "an already-ACKed tx should be dropped by stop-after-ACK"
    );
    node.shutdown();
}
