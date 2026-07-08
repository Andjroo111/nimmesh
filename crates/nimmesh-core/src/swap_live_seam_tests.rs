//! A2b seam tests over the REAL node loop: the driver's `note_peer` reporting (both roles),
//! the session's `note_funding_wire` feed, and a full mesh swap whose responder is gated by
//! the REAL [`crate::nim_verifier::NimHtlcVerifier`] over a deterministic [`MockRpc`] chain —
//! proving the live wiring end to end with zero network.

use std::sync::{Arc, Mutex};

use crate::mock_radio::MeshHarness;
use crate::nim_verifier::{nim_htlc_timeout_ms, NimFundingStore, NimHtlcVerifier};
use crate::nimiq::address::Address;
use crate::nimiq::hex::bytes_to_hex;
use crate::nimiq::htlc::{HashAlgorithm, HtlcCreation, HtlcCreationData};
use crate::rpc::{MockRpc, RpcAccount};
use crate::swap::{LadderParams, SwapPhase};
use crate::swap_coordinator::SwapContext;
use crate::swap_funding_verify::{FundingObservation, FundingVerifier, HtlcExpectation};
use crate::swap_intent::Asset;
use crate::swap_session::SwapSession;
use crate::swap_signer::{MockSigner, SwapSigner};
use crate::swap_wire::{SwapKind, SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};
use crate::test_support::{
    new_initiator_signed, participant_fixtures, wait_until, GIVE_NIM, SETTLE,
};

/// One recorded `note_peer` report: `(swap_id, peer NIM address, peer chain address)`.
type PeerNote = ([u8; SWAP_ID_LEN], [u8; NIM_ADDRESS_LEN], Vec<u8>);

/// A `MockSigner` twin that records every `note_peer` the driver reports.
struct RecordingSigner {
    notes: Arc<Mutex<Vec<PeerNote>>>,
}

impl SwapSigner for RecordingSigner {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        MockSigner.build_funding(ctx, leg)
    }
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        MockSigner.build_claim(ctx, secret)
    }
    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.notes
            .lock()
            .unwrap()
            .push((swap_id, peer_nim_address, peer_chain_address.to_vec()));
    }
}

#[test]
fn the_driver_reports_peer_addressing_to_both_sides_signers() {
    // A full sim swap over the real mesh: alice (initiator) must learn bob's Accept-carried
    // addressing; bob (responder) must learn alice's Propose-carried addressing — each via
    // its own signer's note_peer, keyed by the swap id.
    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let alice_notes = Arc::new(Mutex::new(Vec::new()));
    let bob_notes = Arc::new(Mutex::new(Vec::new()));

    let mut h = MeshHarness::new();
    let alice = h.add_participant_with_signer(
        "alice",
        &[1],
        alice_id.clone(),
        LadderParams::default(),
        Box::new(RecordingSigner {
            notes: alice_notes.clone(),
        }),
    );
    let bob = h.add_participant_with_signer(
        "bob",
        &[2],
        bob_id.clone(),
        LadderParams::default(),
        Box::new(RecordingSigner {
            notes: bob_notes.clone(),
        }),
    );
    h.connect("alice", "bob");

    let (coordinator, propose) =
        new_initiator_signed(alice_ctx.clone(), [42u8; 32], LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    assert!(
        wait_until(
            || alice.swap_phase(swap_id) == Some(SwapPhase::Settled)
                && bob.swap_phase(swap_id) == Some(SwapPhase::Settled),
            SETTLE
        ),
        "the sim swap never settled on both sides"
    );

    // Bob saw alice's Propose addressing (her NIM refund + chain-agnostic claim address).
    let bob_seen = bob_notes.lock().unwrap().clone();
    assert!(
        bob_seen.contains(&(
            swap_id,
            alice_ctx.nim_address,
            alice_ctx.btc_address.clone()
        )),
        "bob's signer never learned the proposer's addressing: {bob_seen:?}"
    );
    // Alice saw bob's Accept addressing (his NIM claim + chain refund address).
    let alice_seen = alice_notes.lock().unwrap().clone();
    assert!(
        alice_seen.contains(&(swap_id, bob_id.nim_address, bob_id.btc_address.clone())),
        "alice's signer never learned the accepter's addressing: {alice_seen:?}"
    );

    h.shutdown();
}

/// A verifier that accepts everything but records every funding wire it is fed.
struct WireRecordingVerifier {
    wires: Arc<Mutex<Vec<NotedWire>>>,
}

/// One recorded funding-wire feed: `(leg, tx_wire)`.
type NotedWire = (SwapLegId, Vec<u8>);

impl FundingVerifier for WireRecordingVerifier {
    fn observe(&self, _expect: &HtlcExpectation) -> FundingObservation {
        FundingObservation::Found {
            amount: u64::MAX,
            timeout: u64::MAX,
            confirmations: u32::MAX,
        }
    }
    fn note_funding_wire(&self, leg: SwapLegId, tx_wire: &[u8]) {
        self.wires.lock().unwrap().push((leg, tx_wire.to_vec()));
    }
}

#[test]
fn the_session_feeds_every_funding_proof_wire_to_the_verifier() {
    // Route a FundingProof through the session directly: the wire must reach the verifier's
    // note seam (leg-tagged) before the gate runs.
    let (swap_id, _alice_id, bob_id, alice_ctx) = participant_fixtures();
    let wires = Arc::new(Mutex::new(Vec::new()));
    let mut session = SwapSession::new(bob_id, LadderParams::default()).with_funding_verifier(
        Box::new(WireRecordingVerifier {
            wires: wires.clone(),
        }),
    );

    let (_coord, propose) = new_initiator_signed(alice_ctx, [42u8; 32], LadderParams::default());
    let propose_bytes = crate::swap_wire::encode_swap(&propose).unwrap();
    session
        .on_message(SwapKind::Propose, &propose_bytes, 0)
        .expect("responder accepts");

    let funding =
        crate::swap_messages::tx_envelope(swap_id, SwapLegId::Nim, vec![0xAB; 200], [0x0F; 32]);
    let funding_bytes = crate::swap_wire::encode_swap(&funding).unwrap();
    session
        .on_message(SwapKind::FundingProof, &funding_bytes, 0)
        .expect("verified funding advances");

    assert_eq!(
        wires.lock().unwrap().clone(),
        vec![(SwapLegId::Nim, vec![0xAB; 200])]
    );
}

#[test]
fn a_mesh_swap_gated_by_the_real_nim_verifier_settles_once_the_chain_confirms() {
    // The A2b wiring end to end, no network: bob's responder session runs the REAL
    // NimHtlcVerifier over a MockRpc "chain". Alice's signer floods a REAL byte-exact NIM
    // HTLC creation as its funding wire. Bob refuses to fund while the chain shows nothing;
    // once the creation is included + the contract account exists at depth, the SAME
    // retransmitted FundingProof passes the gate and the whole swap drives to Settled.
    let (swap_id, alice_id, bob_id, alice_ctx) = participant_fixtures();
    let secret = [42u8; 32];
    let hashlock = crate::swap_leg::sha256(&secret);

    // The REAL funding wire alice's live signer would broadcast (recipient = bob's claim
    // address, ms-mapped T_A timeout) — here signed with a fixture key and flooded as the
    // FundingProof wire by a custom signer.
    let now_ms = 1_000_000u64;
    let funder_key = crate::nimiq::signer::InMemoryEnclaveKey::from_secret(&[0x51; 32]);
    let funder_pk: [u8; 32] = crate::nimiq::signer::EnclaveKey::public_key(&funder_key)
        .try_into()
        .unwrap();
    let funder = Address::from_public_key(&funder_pk);
    let creation = HtlcCreation {
        funder,
        data: HtlcCreationData {
            htlc_sender: funder,
            htlc_recipient: Address::from_bytes(bob_id.nim_address),
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root: hashlock,
            hash_count: 1,
            timeout: nim_htlc_timeout_ms(alice_ctx.terms.nim_timeout, 0, now_ms),
        },
        value: GIVE_NIM,
        fee: 0,
        validity_start_height: 100,
        network_id: crate::NetworkId::Testnet.wire_id(),
    };
    let signature: [u8; 64] =
        crate::nimiq::signer::EnclaveKey::sign_content(&funder_key, creation.serialize_content())
            .try_into()
            .unwrap();
    let wire = creation.serialize_wire(&crate::nimiq::tx::signature_proof_single_sig(
        &funder_pk, &signature,
    ));

    /// Alice-side signer: floods the REAL creation wire for the NIM leg (claim = sim).
    struct RealWireSigner {
        wire: Vec<u8>,
        tx_id: [u8; 32],
    }
    impl SwapSigner for RealWireSigner {
        fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
            match leg {
                SwapLegId::Nim => Some((self.wire.clone(), self.tx_id)),
                SwapLegId::Counterparty => MockSigner.build_funding(ctx, leg),
            }
        }
        fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
            MockSigner.build_claim(ctx, secret)
        }
    }

    // Bob's REAL verifier over the mock chain (deterministic clock near the funder's).
    let rpc = Arc::new(MockRpc::new(100));
    let store = Arc::new(NimFundingStore::new());
    let verifier = NimHtlcVerifier::new(rpc.clone(), store.clone())
        .with_clock(Box::new(move || now_ms + 60_000));
    let bob_session = SwapSession::new(bob_id.clone(), LadderParams::default())
        .with_funding_verifier(Box::new(verifier))
        .with_counterparty_chain(Asset::Usdc);

    let mut h = MeshHarness::new();
    let alice = h.add_participant_with_signer(
        "alice",
        &[1],
        alice_id,
        LadderParams::default(),
        Box::new(RealWireSigner {
            wire: wire.clone(),
            tx_id: creation.tx_hash(),
        }),
    );
    let bob = h.add_session_participant("bob", &[2], bob_session, Box::new(MockSigner));
    h.connect("alice", "bob");

    let (coordinator, propose) = new_initiator_signed(alice_ctx, secret, LadderParams::default());
    alice.start_swap(swap_id, coordinator, propose);

    // Bob accepts, sees the FundingProof — but the chain shows NOTHING, so he must sit at
    // Accepted (never fund) no matter how many retransmits arrive.
    assert!(
        wait_until(
            || bob.swap_phase(swap_id) == Some(SwapPhase::Accepted),
            SETTLE
        ),
        "bob never accepted"
    );
    for _ in 0..5 {
        alice.poll_sync(); // retransmit the FundingProof
        bob.poll_sync();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        bob.swap_phase(swap_id),
        Some(SwapPhase::Accepted),
        "bob advanced on a message alone — the S1 gate is broken"
    );

    // The chain confirms: the creation is included deep enough and the contract account is
    // live with the full amount. The next retransmit passes the gate → the swap completes.
    rpc.confirm(&bytes_to_hex(&creation.tx_hash()), 100);
    rpc.set_head(102); // depth 3 ≥ the testnet NIM policy (2)
    rpc.set_account(
        &creation.contract_address().to_user_friendly(),
        RpcAccount {
            balance: GIVE_NIM,
            account_type: "htlc".to_string(),
            address: Some(creation.contract_address().to_user_friendly()),
        },
    );
    let mut done = false;
    for _ in 0..200 {
        alice.poll_sync();
        bob.poll_sync();
        std::thread::sleep(std::time::Duration::from_millis(20));
        if alice.swap_phase(swap_id).is_none() && bob.swap_phase(swap_id).is_none() {
            done = true; // both Settled → reaped (head stays 0: no stale/refund exit)
            break;
        }
    }
    assert!(done, "the verifier-gated swap did not settle on both sides");

    // And bob's shared store retained the decoded funding — exactly what his live claim uses.
    let rec = store.get(&hashlock).expect("the funding hint was retained");
    assert_eq!(rec.creation.value, GIVE_NIM);
    assert_eq!(rec.contract, creation.contract_address());

    h.shutdown();
}
