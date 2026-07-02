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
    frames: Mutex<Vec<Vec<u8>>>,
    stopped: AtomicBool,
}

impl SpyRadio {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(SpyRadio {
            sends: Mutex::new(Vec::new()),
            frames: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        })
    }
    pub(crate) fn send_count(&self) -> usize {
        self.sends.lock().unwrap().len()
    }
    /// Every raw frame this radio was asked to send — so a test can decode them and count a specific
    /// message type (e.g. how many `SwapIntent` re-advertisements went out).
    pub(crate) fn frames(&self) -> Vec<Vec<u8>> {
        self.frames.lock().unwrap().clone()
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
    fn send(&self, peer_id: String, bytes: Vec<u8>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        self.frames.lock().unwrap().push(bytes);
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

// --- Mesh-swap participant fixtures (shared by the swap node e2e + adversarial suites) -----------

/// Luna the initiator gives on the NIM leg.
pub(crate) const GIVE_NIM: u64 = 100_000;
/// Sats the responder gives on the BTC leg.
pub(crate) const TAKE_BTC: u64 = 50_000;
const T_A: u64 = 10_000; // NIM-leg timeout (longer)
const T_B: u64 = 5_000; // counterparty-leg timeout (shorter)

/// S2 / #73: the NIM identity seed that owns the alice fixture's `nim_address` — its Ed25519 key
/// hashes to [`participant_fixtures`]'s alice address, so alice's Proposes can be authenticated. A
/// signing participant is wired with the matching enclave key ([`alice_propose_key`]).
pub(crate) const ALICE_NIM_SECRET: [u8; 32] = [0x51; 32];

/// The alice fixture's key-derived NIM address (`Blake2b-256(pubkey)[..20]` of [`ALICE_NIM_SECRET`]).
pub(crate) fn alice_nim_address() -> [u8; 20] {
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&ALICE_NIM_SECRET)
        .verifying_key()
        .to_bytes();
    *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes()
}

/// An in-memory [`EnclaveKey`](crate::nimiq::signer::EnclaveKey) for the alice fixture's NIM identity
/// — wire it into a signing participant ([`crate::mock_radio::MeshHarness::add_participant_signing`])
/// so its discovery-origination flow authenticates each Propose it floods.
pub(crate) fn alice_propose_key() -> std::sync::Arc<dyn crate::nimiq::signer::EnclaveKey> {
    std::sync::Arc::new(crate::nimiq::signer::InMemoryEnclaveKey::from_secret(
        &ALICE_NIM_SECRET,
    ))
}

/// Sign a `Propose` envelope under `nim_secret` (the way a node's enclave seam does), returning the
/// wire-signed envelope a responder will authenticate. `nim_secret` must own the Propose's nim_address.
pub(crate) fn sign_propose(
    propose: &crate::swap_wire::SwapEnvelope,
    nim_secret: &[u8; 32],
) -> crate::swap_wire::SwapEnvelope {
    let p = crate::swap_messages::SwapProposal::from_envelope(propose)
        .expect("a well-formed Propose to sign");
    let (pubkey, sig) = p.sign(nim_secret);
    p.to_signed_envelope(pubkey, sig)
}

/// [`SwapCoordinator::new_initiator`](crate::swap_coordinator::SwapCoordinator::new_initiator) plus an
/// authenticated Propose, exactly as a node originates one — the Propose is signed under
/// [`ALICE_NIM_SECRET`] (which owns the alice fixtures' `nim_address`) before it hits the wire.
pub(crate) fn new_initiator_signed(
    ctx: crate::swap_coordinator::SwapContext,
    swap_secret: [u8; 32],
    ladder: crate::swap::LadderParams,
) -> (
    crate::swap_coordinator::SwapCoordinator,
    crate::swap_wire::SwapEnvelope,
) {
    let (coord, propose) =
        crate::swap_coordinator::SwapCoordinator::new_initiator(ctx, swap_secret, ladder);
    (coord, sign_propose(&propose, &ALICE_NIM_SECRET))
}

/// The proven-safe ladder used across the swap node tests.
pub(crate) fn terms() -> crate::swap::SwapTerms {
    crate::swap::SwapTerms {
        nim_timeout: T_A,
        counterparty_timeout: T_B,
    }
}

/// A `(swap_id, alice identity, bob identity, alice's initiator context)` tuple for a node-level
/// swap (alice gives NIM + wants BTC; hashlock = SHA-256 of the `[42; 32]` secret the tests use).
pub(crate) fn participant_fixtures() -> (
    [u8; 16],
    crate::swap_session::NodeIdentity,
    crate::swap_session::NodeIdentity,
    crate::swap_coordinator::SwapContext,
) {
    use crate::swap_coordinator::SwapContext;
    use crate::swap_session::NodeIdentity;
    let swap_id = [0x7A; 16];
    let pk = |b: u8| {
        let mut k = [b; 33];
        k[0] = 0x02;
        k
    };
    let alice_id = NodeIdentity {
        // S2 / #73: key-derived so alice's authenticated Propose self-certifies (its signing pubkey
        // hashes to this address). The matching enclave key is `alice_propose_key`.
        nim_address: alice_nim_address(),
        btc_address: b"tb1qalice".to_vec(),
        btc_pubkey: pk(0x11),
        rate_policy: crate::swap_session::RatePolicy::accept_all(),
        max_concurrent_swaps: crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    };
    let bob_id = NodeIdentity {
        nim_address: [0xB2; 20],
        btc_address: b"tb1qbob".to_vec(),
        btc_pubkey: pk(0x22),
        rate_policy: crate::swap_session::RatePolicy::accept_all(),
        max_concurrent_swaps: crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    };
    let alice_ctx = SwapContext {
        swap_id,
        terms: terms(),
        hashlock: crate::swap_leg::sha256(&[42u8; 32]),
        nim_address: alice_id.nim_address,
        btc_address: alice_id.btc_address.clone(),
        btc_pubkey: alice_id.btc_pubkey,
        give_amount: GIVE_NIM,
        take_amount: TAKE_BTC,
        network_id: 5,
    };
    (swap_id, alice_id, bob_id, alice_ctx)
}
