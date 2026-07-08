//! # send_e2e_tests — C1: the real-signed money path, end to end (`cfg(test)` only)
//!
//! The G3 signer, the G4 wire, the G6 relay, the G8 gateway, the G12 verify-before-relay
//! filter, and the G17 settlement ledger, exercised together with a **genuine Ed25519
//! signature** (not the opaque stand-in bytes the other suites flood). This is the headless
//! proof that the C1 send path is real: build → **sign** with an `AppSigner` over an
//! `EnclaveKey` (the seed never leaves it) → flood → a **verifying** relay accepts it
//! *because* the signature is valid → the gateway broadcasts the exact signed bytes → the
//! origin settles. **Testnet only.**

use std::sync::Arc;

use crate::default_network;
use crate::gateway::MockGateway;
use crate::mock_radio::MeshHarness;
use crate::nimiq::signer::{signed_transfer_wire, InMemoryEnclaveKey};
use crate::nimiq::{AppSigner, TransferIntent};
use crate::settlement::PaymentStatus;
use crate::test_support::SETTLE;

/// A real testnet transfer to a known-valid recipient (the signer-test fixture address).
fn intent(value: u64, vsh: u32) -> TransferIntent {
    TransferIntent {
        recipient: "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".to_string(),
        value,
        validity_start_height: vsh,
        network: default_network(),
    }
}

#[test]
fn a_real_signed_transfer_floods_through_a_verifying_relay_and_settles() {
    // The app's key — a native enclave in production, in-memory here. The seed never leaves it;
    // only the public key (for the address) and the signature ever come out.
    let signer = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&[3u8; 32])));
    assert!(signer.address().unwrap().starts_with("NQ"));

    // Build + sign a real basic transfer (139-byte wire). anchored_intent (head-beacon
    // anchoring) is covered in beacon_e2e_tests; here we fix the validity height.
    let signed = signer.sign_transfer(intent(250_000, 100)).unwrap();
    assert_eq!(signed.raw_hex.len() / 2, 139);
    let wire = signed_transfer_wire(&signed).unwrap();

    // origin -> VERIFYING relay -> gateway. The relay runs the G12 spam filter, so the only
    // thing that crosses is a genuinely-signed tx — which this is.
    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_verifying_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let tx_id = origin.submit_signed_transfer(signed);

    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Settled);
    assert_eq!(gw.submission_count(), 1);
    assert_eq!(gw.submissions()[0], wire); // the gateway broadcast exactly our signed bytes
    assert_eq!(relay.verify_dropped(), 0); // a real signature passes verify-before-relay

    h.shutdown();
}

#[test]
fn a_tampered_signed_transfer_is_dropped_by_a_verifying_relay() {
    // Sign a real transfer, then flip one hex nibble in the value field so the signature no
    // longer matches — a forger's "I changed the amount after signing." The verifying relay
    // must drop it (no broadcast, origin never settles).
    let signer = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&[4u8; 32])));
    let mut signed = signer.sign_transfer(intent(1_000, 50)).unwrap();

    // The value field is wire bytes 54..62 → hex chars 108..124. Flip one, keeping valid hex.
    let mut hex = signed.raw_hex.into_bytes();
    hex[110] = if hex[110] == b'0' { b'1' } else { b'0' };
    signed.raw_hex = String::from_utf8(hex).unwrap();

    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let relay = h.add_verifying_node("relay", &[2]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    h.connect("origin", "relay");
    h.connect("relay", "gw");

    let tx_id = origin.submit_signed_transfer(signed);

    assert_eq!(origin.wait_payment(&tx_id, SETTLE), PaymentStatus::Pending);
    assert_eq!(gw.submission_count(), 0);
    assert!(relay.verify_dropped() >= 1, "a tampered tx must be dropped");

    h.shutdown();
}

#[test]
fn a_send_into_the_void_delivers_when_the_mesh_reappears() {
    // The drive-away scenario (data-mule): sign with NO peers in range, meet the mesh
    // later, and the pending tx re-floods on the next tick and settles. Before
    // `pending_retry` this send was lost the moment the initial flood hit nobody.
    let signer = AppSigner::new(Arc::new(InMemoryEnclaveKey::from_secret(&[5u8; 32])));
    let signed = signer.sign_transfer(intent(42_000, 77)).unwrap();

    let mut h = MeshHarness::new();
    let gw = Arc::new(MockGateway::new(default_network()));
    let origin = h.add_node("origin", &[1]);
    let _gateway = h.add_gateway("gw", &[3], gw.clone());
    // NOT connected yet: the initial flood reaches nobody.
    let tx_id = origin.submit_signed_transfer(signed);
    assert_eq!(gw.submission_count(), 0);

    // Drive home: the mesh appears, and the ~15s heartbeat tick re-offers the tx
    // immediately (a void-flooded tx skips the retry cadence on its first chance).
    // Fence-driven drain (ADR-0005) rather than a wall-clock wait — the beacon re-flood →
    // gateway submit → receipt round-trip is deterministic once the workers quiesce, so
    // this no longer races CI's 2-core oversubscription (the old `wait_payment` did).
    h.connect("origin", "gw");
    origin.poll_beacon();
    for _ in 0..3 {
        origin.fence();
        h.ether().fence();
        _gateway.fence();
        h.ether().fence();
        origin.fence();
    }

    assert_eq!(
        origin.payment_status(tx_id.clone()),
        PaymentStatus::Settled,
        "the pending tx must settle once the mesh reappears"
    );
    assert_eq!(gw.submission_count(), 1);
    h.shutdown();
}
