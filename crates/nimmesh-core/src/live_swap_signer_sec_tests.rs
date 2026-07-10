//! G8-review regression tests for the live signer's money-path hardening (offline, no
//! network): C1 (a live signer can never ride an unsafe session through ANY constructor),
//! M3 (the reveal is held until the withdraw is buried, and a landed withdraw is never
//! rebroadcast), M4 (implausible absolute timelocks are refused before funds move; the
//! verifier maps against the expectation's own anchor). Fixtures shared with
//! [`super::tests`].

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::tests::{
    ctx, hashlock, initiator, responder, MockAmoy, ALICE_EVM_CLAIM, NOW_MS, NOW_S, SECRET,
};
use super::*;
use crate::mock_radio::{MockEther, MockRadio};
use crate::nim_verifier::NimHtlcVerifier;
use crate::node::MeshNode;
use crate::rpc::MockRpc;
use crate::swap::LadderParams;
use crate::swap_funding_verify::{FundingObservation, FundingVerifier};
use crate::swap_rate::RatePolicy;
use crate::swap_session::{NodeIdentity, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS};

// --- M4: the absolute-timelock sanity bound ---------------------------------------------------

#[test]
fn initiator_refuses_an_implausible_nim_timelock_before_any_broadcast() {
    let nim_rpc = Arc::new(MockRpc::new(777));
    let signer = initiator(
        nim_rpc.clone(),
        Arc::new(MockAmoy::default()),
        Arc::new(PeerBook::new()),
        Arc::new(PolygonFundingStore::new()),
    );
    signer.note_peer(ctx().swap_id, [0xB2; NIM_ADDRESS_LEN], b"x");

    // A mis-anchored term (anchor 0 against a "real head" term in the millions) maps to a
    // multi-week lock — refused before any funds move (M4).
    let mut weeks = ctx();
    weeks.terms.nim_timeout = 3_000_000 + 10_000; // term far beyond the 6 h cap at anchor 0
    weeks.term_anchor = 0;
    assert!(signer.build_funding(&weeks, SwapLegId::Nim).is_none());

    // A degenerate term at/behind the anchor maps to an already-expired lock — refused.
    let mut instant = ctx();
    instant.terms.nim_timeout = 5;
    instant.term_anchor = 10;
    assert!(signer.build_funding(&instant, SwapLegId::Nim).is_none());
    assert!(nim_rpc.broadcasts().is_empty());

    // The SAME huge term with the MATCHING anchor is the honest ~2.8 h lock — accepted.
    let mut anchored = ctx();
    anchored.terms.nim_timeout = 3_000_000 + 10_000;
    anchored.term_anchor = 3_000_000;
    assert!(signer.build_funding(&anchored, SwapLegId::Nim).is_some());
    assert_eq!(nim_rpc.broadcasts().len(), 1);
}

#[test]
fn responder_refuses_an_implausible_usdc_timelock_before_any_broadcast() {
    let amoy = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        balance: u128::MAX,
        auto_mine_block: Some(1000),
        ..MockAmoy::default()
    });
    let book = Arc::new(PeerBook::new());
    let signer = responder(amoy.clone(), book, Arc::new(NimFundingStore::new()));
    signer.note_peer(ctx().swap_id, [0xA1; NIM_ADDRESS_LEN], &ALICE_EVM_CLAIM);

    let mut weeks = ctx();
    weeks.terms.counterparty_timeout = 3_000_000 + 5_000;
    weeks.term_anchor = 0;
    assert!(signer
        .build_funding(&weeks, SwapLegId::Counterparty)
        .is_none());

    let mut instant = ctx();
    instant.terms.counterparty_timeout = 5;
    instant.term_anchor = 10;
    assert!(signer
        .build_funding(&instant, SwapLegId::Counterparty)
        .is_none());
    assert!(amoy.broadcasts().is_empty());
}

// --- M3: hold the reveal until the withdraw is buried; never rebroadcast ------------------------

#[test]
fn the_reveal_is_held_until_the_withdraw_is_buried_and_never_rebroadcast() {
    let amoy = Arc::new(MockAmoy {
        gas_price: 30_000_000_000,
        nonce: 3,
        balance: u128::MAX,
        auto_mine_block: Some(2000),
        ..MockAmoy::default() // head 0 → the mined withdraw sits at depth < 2
    });
    let store = Arc::new(PolygonFundingStore::new());
    let escrow_id = [0x9F; 32];
    store.record_found(
        hashlock(),
        FoundEscrow {
            swap_id: escrow_id,
            amount: 1_000_000,
            timelock_s: NOW_S + 5_000,
        },
    );
    let signer = initiator(
        Arc::new(MockRpc::new(1)),
        amoy.clone(),
        Arc::new(PeerBook::new()),
        store,
    );

    // The withdraw lands (1 broadcast) but is NOT buried to the reveal depth → S is held
    // off the mesh (None).
    assert!(signer.build_claim(&ctx(), SECRET).is_none());
    assert_eq!(amoy.broadcasts().len(), 1);

    // A retry while still shallow must NOT rebroadcast a second withdraw.
    assert!(signer.build_claim(&ctx(), SECRET).is_none());
    assert_eq!(amoy.broadcasts().len(), 1);

    // Once the chain buries it (depth ≥ 2) and the escrow reads Claimed, the SAME landed
    // claim is replayed — S first, still exactly one broadcast ever.
    amoy.head.store(2001, Ordering::Relaxed);
    amoy.swap_states.lock().unwrap().insert(escrow_id, 2); // Claimed
    let (wire, _) = signer.build_claim(&ctx(), SECRET).expect("reveal releases");
    assert_eq!(&wire[..32], &SECRET);
    assert_eq!(amoy.broadcasts().len(), 1);
}

// --- M4: the verifier maps against the expectation's own anchor --------------------------------

#[test]
fn the_amoy_verifier_maps_the_timelock_against_the_expectations_anchor() {
    use super::tests::HTLC;
    let chain = Arc::new(MockAmoy {
        head: std::sync::atomic::AtomicU64::new(200),
        ..MockAmoy::default()
    });
    let store = Arc::new(PolygonFundingStore::new());
    let v = AmoyHtlcSwapVerifier::new(chain.clone(), HTLC, ALICE_EVM_CLAIM, store)
        .with_clock(Box::new(|| NOW_S));
    let raw = vec![0xF0; 100];
    v.note_funding_wire(SwapLegId::Counterparty, &raw);
    chain.add_receipt(crate::evm::keccak256(&raw), true, 100);
    chain.logs.lock().unwrap().push(super::tests::new_swap_log(
        [0x9F; 32],
        1_000_000,
        hashlock(),
        NOW_S + 5_000,
        100,
    ));
    chain.swap_states.lock().unwrap().insert([0x9F; 32], 1);

    let mut expect = super::tests::counterparty_expect();
    expect.term_anchor = 2_000; // the swap's terms were minted against head 2_000
    match v.observe(&expect) {
        FundingObservation::Found { timeout, .. } => {
            // (timelock − now) + slack + ANCHOR — the anchor rides the expectation (M4).
            assert_eq!(timeout, 5_000 + DEFAULT_TIMELOCK_SLACK_S + 2_000);
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

// --- C1: a live signer can never ride an unsafe session ----------------------------------------

fn live_initiator_signer() -> LiveInitiatorSigner {
    initiator(
        Arc::new(MockRpc::new(1)),
        Arc::new(MockAmoy::default()),
        Arc::new(PeerBook::new()),
        Arc::new(PolygonFundingStore::new()),
    )
}

fn identity() -> NodeIdentity {
    let mut pk = [0x22; 33];
    pk[0] = 0x02;
    NodeIdentity {
        nim_address: [0xB2; 20],
        btc_address: ALICE_EVM_CLAIM.to_vec(),
        btc_pubkey: pk,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
        standing_intent: None,
    }
}

#[test]
fn live_signers_declare_themselves_and_the_sim_ones_do_not() {
    assert!(SwapSigner::is_live(&live_initiator_signer()));
    let resp = responder(
        Arc::new(MockAmoy::default()),
        Arc::new(PeerBook::new()),
        Arc::new(NimFundingStore::new()),
    );
    assert!(SwapSigner::is_live(&resp));
    assert!(!SwapSigner::is_live(&crate::swap_signer::MockSigner));
}

#[test]
#[should_panic(expected = "refusing to build a live-signer node")]
fn building_a_live_signer_over_the_default_session_panics() {
    // The C1 invariant at the LAST door: `SwapSession::new`'s defaults (AcceptAllVerifier +
    // sim secret source) must never guard real funds — even through the rig constructor.
    let radio = MockRadio::new("c1", MockEther::new());
    let session = SwapSession::new(identity(), LadderParams::default());
    let _ = MeshNode::new_session_participant(
        b"c1".to_vec(),
        radio,
        session,
        Box::new(live_initiator_signer()),
    );
}

#[test]
fn building_a_live_signer_over_a_live_safe_session_constructs() {
    let radio = MockRadio::new("c1ok", MockEther::new());
    let session = SwapSession::new(identity(), LadderParams::default())
        .with_funding_verifier(Box::new(NimHtlcVerifier::new(
            Arc::new(MockRpc::new(1)),
            Arc::new(NimFundingStore::new()),
        )))
        .with_secret_source(Box::new(|swap_id| crate::swap_leg::sha256(swap_id)));
    let node = MeshNode::new_session_participant(
        b"c1ok".to_vec(),
        radio,
        session,
        Box::new(live_initiator_signer()),
    );
    node.shutdown();
}

#[test]
fn chain_backed_is_true_only_for_the_real_verifiers() {
    // Fail-closed default: the sim accept-all can never be live-eligible.
    assert!(!crate::swap_funding_verify::AcceptAllVerifier.chain_backed());
    let nim = NimHtlcVerifier::new(Arc::new(MockRpc::new(1)), Arc::new(NimFundingStore::new()));
    assert!(FundingVerifier::chain_backed(&nim));
    let amoy = AmoyHtlcSwapVerifier::new(
        Arc::new(MockAmoy::default()),
        super::tests::HTLC,
        ALICE_EVM_CLAIM,
        Arc::new(PolygonFundingStore::new()),
    );
    assert!(FundingVerifier::chain_backed(&amoy));
}

// --- M4: the funder-side mapping uses the context anchor (byte-level assert) --------------------

#[test]
fn fund_nim_maps_the_timeout_against_the_context_anchor() {
    let nim_rpc = Arc::new(MockRpc::new(777));
    let signer = initiator(
        nim_rpc.clone(),
        Arc::new(MockAmoy::default()),
        Arc::new(PeerBook::new()),
        Arc::new(PolygonFundingStore::new()),
    );
    signer.note_peer(ctx().swap_id, [0xB2; NIM_ADDRESS_LEN], b"x");
    let mut c = ctx();
    c.terms.nim_timeout = 2_000 + 10_000;
    c.term_anchor = 2_000; // terms minted against head 2_000
    let (wire, _) = signer.build_funding(&c, SwapLegId::Nim).expect("funds");
    let creation = crate::nimiq::htlc::decode_creation_wire(&wire).expect("decodes");
    // now + (term − anchor)·1000 — the anchor comes from the CONTEXT, not a config field.
    assert_eq!(creation.data.timeout, NOW_MS + 10_000 * 1000);
}
