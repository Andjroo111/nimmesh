//! Offline tests for [`crate::nim_verifier`] — the NIM-RPC-backed funding gate against a
//! deterministic [`MockRpc`] chain: the hint→chain-truth pipeline, every refusal path
//! (fail-closed), the ledger-reference mismatch semantics, and the ADR-0010 timeout mapping.

use std::sync::Arc;

use super::*;
use crate::nimiq::address::ADDRESS_LEN;
use crate::nimiq::htlc::HtlcCreationData;
use crate::rpc::{MockRpc, RpcAccount};
use crate::swap_funding_verify::{require_funded, FundingRejected};

const HASH_SECRET: [u8; 32] = [0x42; 32];
/// The verifier's fixed test clock (ms) — 500 s after the funder's mapping clock below.
const VERIFY_NOW_MS: u64 = 1_500_000;
/// The funder's mapping clock (ms).
const FUND_NOW_MS: u64 = 1_000_000;
/// The agreed NIM-leg term (`T_A`, term units ≈ seconds).
const T_A: u64 = 10_000;
const AMOUNT: u64 = 500_000;

fn hashlock() -> [u8; 32] {
    crate::swap_leg::sha256(&HASH_SECRET)
}

fn recipient() -> [u8; ADDRESS_LEN] {
    [0xB2; ADDRESS_LEN]
}

/// The honest funding tx an initiator would build: recipient = the responder's claim address,
/// timeout mapped from `T_A` at the funder's clock (ADR-0010).
fn creation() -> HtlcCreation {
    HtlcCreation {
        funder: Address::from_bytes([0xA1; ADDRESS_LEN]),
        data: HtlcCreationData {
            htlc_sender: Address::from_bytes([0xA1; ADDRESS_LEN]),
            htlc_recipient: Address::from_bytes(recipient()),
            hash_algorithm: HashAlgorithm::Sha256,
            hash_root: hashlock(),
            hash_count: 1,
            timeout: nim_htlc_timeout_ms(T_A, 0, FUND_NOW_MS),
        },
        value: AMOUNT,
        fee: 0,
        validity_start_height: 100,
        network_id: crate::NetworkId::Testnet.wire_id(),
    }
}

fn expect() -> HtlcExpectation {
    HtlcExpectation {
        leg: SwapLegId::Nim,
        hashlock: hashlock(),
        min_amount: AMOUNT,
        min_timeout: T_A,
        recipient: recipient().to_vec(),
        term_anchor: 0,
    }
}

fn verifier_over(rpc: Arc<MockRpc>) -> NimHtlcVerifier {
    NimHtlcVerifier::new(rpc, Arc::new(NimFundingStore::new()))
        .with_clock(Box::new(|| VERIFY_NOW_MS))
}

/// Seed the mock chain with the creation included at `block` and the contract account live
/// with `balance`, head at `head`.
fn seed_chain(rpc: &MockRpc, c: &HtlcCreation, block: u32, head: u32, balance: u64) {
    rpc.set_head(head);
    rpc.confirm(&bytes_to_hex(&c.tx_hash()), block);
    rpc.set_account(
        &c.contract_address().to_user_friendly(),
        RpcAccount {
            balance,
            account_type: "htlc".to_string(),
            address: Some(c.contract_address().to_user_friendly()),
        },
    );
}

#[test]
fn a_correct_deep_funding_verifies_end_to_end() {
    let rpc = Arc::new(MockRpc::new(105));
    let c = creation();
    seed_chain(&rpc, &c, 100, 105, AMOUNT);
    let v = verifier_over(rpc);
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));

    let obs = v.observe(&expect());
    match obs {
        FundingObservation::Found {
            amount,
            timeout,
            confirmations,
        } => {
            assert_eq!(amount, AMOUNT);
            assert_eq!(confirmations, 6); // 105 - 100 + 1
                                          // (timeout_ms − now)/1000 + slack: (11_000_000 − 1_500_000)/1000 + 900 = 10_400.
            assert_eq!(timeout, 10_400);
        }
        other => panic!("expected Found, got {other:?}"),
    }
    // The full gate passes at the testnet NIM depth (2).
    assert!(require_funded(&obs, &expect(), 2).is_ok());
}

#[test]
fn a_returned_tx_whose_hash_is_not_the_content_digest_is_refused() {
    // M5 content-hash bind: a lying/confused node echoes the queried hash back as a DIFFERENT
    // reported hash while attaching a real-looking inclusion height. The verifier recomputes
    // Blake2b(content) and refuses any returned tx whose identity is not that digest → Absent
    // (never trusts the fabricated height), even though the contract account is fully funded.
    let rpc = Arc::new(MockRpc::new(105));
    let c = creation();
    // Contract account is live + funded (so ONLY the tx-hash bind can refuse it).
    rpc.set_head(105);
    rpc.set_account(
        &c.contract_address().to_user_friendly(),
        RpcAccount {
            balance: AMOUNT,
            account_type: "htlc".to_string(),
            address: Some(c.contract_address().to_user_friendly()),
        },
    );
    // The node answers getTransactionByHash(our digest) with a tx that reports a FOREIGN hash.
    rpc.confirm_as(&bytes_to_hex(&c.tx_hash()), &"ab".repeat(32), 100);
    let v = verifier_over(rpc.clone());
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);

    // Control: the SAME chain but the node reports the honest (matching) hash → Found.
    rpc.confirm(&bytes_to_hex(&c.tx_hash()), 100);
    assert!(matches!(
        v.observe(&expect()),
        FundingObservation::Found { .. }
    ));
}

#[test]
fn no_hint_reads_absent_fail_closed() {
    let rpc = Arc::new(MockRpc::new(105));
    let c = creation();
    seed_chain(&rpc, &c, 100, 105, AMOUNT); // chain is fine — but no FundingProof arrived
    let v = verifier_over(rpc);
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);
    assert_eq!(
        require_funded(&v.observe(&expect()), &expect(), 2),
        Err(FundingRejected::NotFundedYet)
    );
}

#[test]
fn an_unincluded_or_unknown_funding_tx_never_advances() {
    // The wire decodes fine but the node has never seen the tx → Absent (a FundingProof for a
    // never-broadcast tx is exactly the S1 attack this gate exists for).
    let rpc = Arc::new(MockRpc::new(105));
    let v = verifier_over(rpc);
    v.note_funding_wire(SwapLegId::Nim, &creation().serialize_wire(&[0u8; 98]));
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);
}

#[test]
fn a_shallow_inclusion_is_too_shallow_until_buried() {
    let rpc = Arc::new(MockRpc::new(100));
    let c = creation();
    seed_chain(&rpc, &c, 100, 100, AMOUNT); // depth 1 < NIM policy 2
    let v = verifier_over(rpc.clone());
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(
        require_funded(&v.observe(&expect()), &expect(), 2),
        Err(FundingRejected::TooShallow { have: 1, need: 2 })
    );
    rpc.set_head(101); // one more block buries it to depth 2
    assert!(require_funded(&v.observe(&expect()), &expect(), 2).is_ok());
}

#[test]
fn a_resolved_or_underheld_contract_is_not_funding() {
    // Claimed (emptied) HTLC account → Absent; an account that isn't an HTLC at all → Absent.
    let rpc = Arc::new(MockRpc::new(105));
    let c = creation();
    seed_chain(&rpc, &c, 100, 105, 0); // included, but the contract has been emptied
    let v = verifier_over(rpc.clone());
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);

    // The MockRpc default answer (a zero-balance BASIC account) also refuses: wrong type.
    let rpc2 = Arc::new(MockRpc::new(105));
    rpc2.set_head(105);
    rpc2.confirm(&bytes_to_hex(&c.tx_hash()), 100);
    let v2 = verifier_over(rpc2);
    v2.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(v2.observe(&expect()), FundingObservation::Absent);
}

#[test]
fn wrong_recipient_and_wrong_hashlock_hints_are_hard_mismatches() {
    let rpc = Arc::new(MockRpc::new(105));
    // A funding paying someone ELSE under our hashlock → Mismatch(Recipient).
    let mut wrong_rec = creation();
    wrong_rec.data.htlc_recipient = Address::from_bytes([0xEE; ADDRESS_LEN]);
    seed_chain(&rpc, &wrong_rec, 100, 105, AMOUNT);
    let v = verifier_over(rpc);
    v.note_funding_wire(SwapLegId::Nim, &wrong_rec.serialize_wire(&[0u8; 98]));
    assert_eq!(
        v.observe(&expect()),
        FundingObservation::Mismatch(MismatchReason::Recipient)
    );

    // A funding paying US under a different hashlock → Mismatch(Hashlock).
    let rpc2 = Arc::new(MockRpc::new(105));
    let mut other_lock = creation();
    other_lock.data.hash_root = crate::swap_leg::sha256(&[0x99; 32]);
    seed_chain(&rpc2, &other_lock, 100, 105, AMOUNT);
    let v2 = verifier_over(rpc2);
    v2.note_funding_wire(SwapLegId::Nim, &other_lock.serialize_wire(&[0u8; 98]));
    assert_eq!(
        v2.observe(&expect()),
        FundingObservation::Mismatch(MismatchReason::Hashlock)
    );
}

#[test]
fn underfunding_and_a_too_short_timeout_are_rejected_by_the_gate() {
    // Locked less than agreed → Underfunded (the classic lie, caught from the bound decode).
    let rpc = Arc::new(MockRpc::new(105));
    let mut small = creation();
    small.value = AMOUNT - 1;
    seed_chain(&rpc, &small, 100, 105, AMOUNT - 1);
    let v = verifier_over(rpc);
    v.note_funding_wire(SwapLegId::Nim, &small.serialize_wire(&[0u8; 98]));
    assert_eq!(
        require_funded(&v.observe(&expect()), &expect(), 2),
        Err(FundingRejected::Underfunded {
            have: AMOUNT - 1,
            need: AMOUNT
        })
    );

    // An on-chain timeout mapped too close to now → TimeoutTooShort. Same honest creation, but
    // the verifier's clock has drifted far past the slack the mapping allows.
    let rpc2 = Arc::new(MockRpc::new(105));
    let c = creation();
    seed_chain(&rpc2, &c, 100, 105, AMOUNT);
    let late = NimHtlcVerifier::new(rpc2, Arc::new(NimFundingStore::new()))
        .with_clock(Box::new(|| FUND_NOW_MS + 2_000_000)); // 2000 s later; slack only 900
    late.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert!(matches!(
        require_funded(&late.observe(&expect()), &expect(), 2),
        Err(FundingRejected::TimeoutTooShort { .. })
    ));
}

#[test]
fn transport_failures_wrong_network_and_foreign_legs_read_absent() {
    let rpc = Arc::new(MockRpc::new(105));
    let c = creation();
    seed_chain(&rpc, &c, 100, 105, AMOUNT);
    let v = verifier_over(rpc.clone());
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));

    // Fail-closed on transport: every read errors → Absent, never an advance.
    rpc.fail_transient("node down");
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);
    rpc.recover();
    assert!(matches!(
        v.observe(&expect()),
        FundingObservation::Found { .. }
    ));

    // The counterparty leg is not this verifier's chain.
    let mut foreign = expect();
    foreign.leg = SwapLegId::Counterparty;
    assert_eq!(v.observe(&foreign), FundingObservation::Absent);

    // A funding stamped for another network is never ours to advance on.
    let rpc2 = Arc::new(MockRpc::new(105));
    let mut mainnet = creation();
    mainnet.network_id = 24; // Albatross mainnet wire id — refused by the testnet pin
    seed_chain(&rpc2, &mainnet, 100, 105, AMOUNT);
    let v2 = verifier_over(rpc2);
    v2.note_funding_wire(SwapLegId::Nim, &mainnet.serialize_wire(&[0u8; 98]));
    assert_eq!(v2.observe(&expect()), FundingObservation::Absent);
}

#[test]
fn the_store_is_first_hint_wins_and_ignores_malformed_wires() {
    let store = NimFundingStore::new();
    store.note_wire(&[0xFF; 40]); // not a creation wire — dropped
    assert!(store.get(&hashlock()).is_none());

    let honest = creation();
    store.note_wire(&honest.serialize_wire(&[0u8; 98]));
    // A second wire under the SAME hashlock (different amount) must not replace the first.
    let mut second = creation();
    second.value = 1;
    store.note_wire(&second.serialize_wire(&[0u8; 98]));
    assert_eq!(store.get(&hashlock()).unwrap().creation.value, AMOUNT);

    // Only the SHA-256 / hash-count-1 cross-chain shape is retained.
    let mut blake = creation();
    blake.data.hash_root = [0xAB; 32];
    blake.data.hash_algorithm = HashAlgorithm::Blake2b;
    store.note_wire(&blake.serialize_wire(&[0u8; 98]));
    assert!(store.get(&[0xAB; 32]).is_none());
}

#[test]
fn the_timeout_mapping_round_trips_with_slack() {
    // Funder side: T = 10_000 at anchor 0, clock 1_000_000 ms → on-chain 11_000_000 ms.
    let on_chain = nim_htlc_timeout_ms(10_000, 0, 1_000_000);
    assert_eq!(on_chain, 11_000_000);
    // Verifier 500 s later with 900 s slack → 10_400 term units ≥ the agreed 10_000.
    assert_eq!(
        term_equivalent_of_timeout_ms(on_chain, 1_500_000, 0, 900),
        10_400
    );
    // A non-zero anchor shifts both sides identically.
    assert_eq!(nim_htlc_timeout_ms(10_000, 4_000, 1_000_000), 7_000_000);
    assert_eq!(
        term_equivalent_of_timeout_ms(7_000_000, 1_500_000, 4_000, 900),
        10_400
    );
    // Saturation: a timeout already in the past maps to just the slack + anchor.
    assert_eq!(term_equivalent_of_timeout_ms(1_000, 2_000, 0, 900), 900);
}
