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

fn with_secondary(primary: Arc<MockRpc>, secondary: Arc<MockRpc>) -> NimHtlcVerifier {
    NimHtlcVerifier::new(primary, Arc::new(NimFundingStore::new()))
        .with_clock(Box::new(|| VERIFY_NOW_MS))
        .with_secondary(secondary)
}

#[test]
fn a_secondary_that_disagrees_on_inclusion_fails_closed_and_agreement_passes() {
    // M5 cross-read: the primary is honest (tx at block 100, funded, head 105), but the trusted
    // depth is only reported when an INDEPENDENT endpoint agrees on the inclusion block.
    let c = creation();
    let primary = Arc::new(MockRpc::new(105));
    seed_chain(&primary, &c, 100, 105, AMOUNT);
    let digest = bytes_to_hex(&c.tx_hash());

    // (a) secondary has never seen the tx → cross-read fails closed.
    let sec_absent = Arc::new(MockRpc::new(105));
    let v = with_secondary(primary.clone(), sec_absent);
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(v.observe(&expect()), FundingObservation::Absent);

    // (b) secondary disagrees on the inclusion block (says 50, not 100) → fail-closed.
    let sec_wrong = Arc::new(MockRpc::new(105));
    sec_wrong.confirm(&digest, 50);
    let v2 = with_secondary(primary.clone(), sec_wrong);
    v2.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert_eq!(v2.observe(&expect()), FundingObservation::Absent);

    // (c) secondary AGREES on the block (100) → the cross-checked observation is Found.
    let sec_ok = Arc::new(MockRpc::new(106));
    sec_ok.confirm(&digest, 100);
    let v3 = with_secondary(primary, sec_ok);
    v3.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    assert!(matches!(
        v3.observe(&expect()),
        FundingObservation::Found { .. }
    ));
}

#[test]
fn a_shallower_secondary_head_defeats_a_primary_that_inflates_depth() {
    // The primary lies about `head` (10_000) to fake a huge depth; the tx is really at block 100.
    // The honest secondary agrees on the block but reports head 100 — the cross-read folds in the
    // CONSERVATIVE head, so the depth reads shallow and the gate refuses it (never over-advances).
    let c = creation();
    let primary = Arc::new(MockRpc::new(10_000));
    seed_chain(&primary, &c, 100, 10_000, AMOUNT);
    let sec = Arc::new(MockRpc::new(100));
    sec.confirm(&bytes_to_hex(&c.tx_hash()), 100);
    let v = with_secondary(primary, sec);
    v.note_funding_wire(SwapLegId::Nim, &c.serialize_wire(&[0u8; 98]));
    match v.observe(&expect()) {
        FundingObservation::Found { confirmations, .. } => assert_eq!(confirmations, 1), // 100-100+1
        other => panic!("expected a conservative depth-1 Found, got {other:?}"),
    }
    assert!(matches!(
        require_funded(&v.observe(&expect()), &expect(), 2),
        Err(FundingRejected::TooShallow { .. })
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

#[test]
fn observe_claim_reports_inclusion_depth_notfound_and_unavailable_fail_closed() {
    // Run-4 fix: the live claim watch. A POSITIVE `getTransactionByHash` consult of our OWN
    // broadcast redeem — three-valued so run 4's transient 429 is DISTINGUISHABLE from "the
    // chain does not know this tx" (conflating them is exactly how a 429 became a fake settle).
    let rpc = Arc::new(MockRpc::new(0));
    let verifier = NimHtlcVerifier::new(rpc.clone(), Arc::new(NimFundingStore::new()));
    let tx = [0x5A; 32];
    let hex = bytes_to_hex(&tx);

    // The chain affirmatively does not know the tx → NotFound (the caller may re-claim).
    assert_eq!(
        verifier.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::NotFound
    );
    // The wrong leg → Unavailable (this verifier speaks only the NIM leg).
    assert_eq!(
        verifier.observe_claim(SwapLegId::Counterparty, &tx),
        ClaimObservation::Unavailable
    );

    // Included at block 100 with head 101 → depth 2 (inclusive count, like the funding read).
    rpc.confirm(&hex, 100);
    rpc.set_head(101);
    assert_eq!(
        verifier.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Included { confirmations: 2 }
    );

    // Run 4's exact failure: a transport error must read Unavailable, never a verdict.
    rpc.fail_transient("rpc http 429 (getTransactionByHash)");
    assert_eq!(
        verifier.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Unavailable
    );
    rpc.recover();

    // M5 content bind: a returned tx whose REPORTED hash is not the queried digest is trusted
    // for nothing — neither an inclusion nor a NotFound.
    let liar = Arc::new(MockRpc::new(101));
    liar.confirm_as(&hex, &bytes_to_hex(&[0x77u8; 32]), 100);
    let lied_to = NimHtlcVerifier::new(liar, Arc::new(NimFundingStore::new()));
    assert_eq!(
        lied_to.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Unavailable
    );
}

#[test]
fn observe_claim_with_a_secondary_needs_agreement_and_takes_the_conservative_depth() {
    // M5 cross-read on the claim watch: a single endpoint can neither fake an inclusion (which
    // authorizes a settle) nor a NotFound (which re-arms a re-broadcast) once a secondary is
    // configured — disagreement of any kind reads Unavailable, and the shallower head wins.
    let tx = [0x5B; 32];
    let hex = bytes_to_hex(&tx);

    let primary = Arc::new(MockRpc::new(120));
    primary.confirm(&hex, 100);
    let secondary = Arc::new(MockRpc::new(105));
    let verifier = NimHtlcVerifier::new(primary.clone(), Arc::new(NimFundingStore::new()))
        .with_secondary(secondary.clone());

    // The secondary does not know the tx yet → no verdict.
    assert_eq!(
        verifier.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Unavailable
    );
    // Agreement on the inclusion block → included; depth from the CONSERVATIVE head
    // (min(120, 105) = 105 → 105 − 100 + 1 = 6, not the primary's 21).
    secondary.confirm(&hex, 100);
    assert_eq!(
        verifier.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Included { confirmations: 6 }
    );
    // Disagreement on the inclusion block → fail-closed again.
    let disagreeing = Arc::new(MockRpc::new(105));
    disagreeing.confirm(&hex, 99);
    let split =
        NimHtlcVerifier::new(primary, Arc::new(NimFundingStore::new())).with_secondary(disagreeing);
    assert_eq!(
        split.observe_claim(SwapLegId::Nim, &tx),
        ClaimObservation::Unavailable
    );

    // And a NotFound now needs BOTH endpoints to say unknown.
    let p2 = Arc::new(MockRpc::new(10));
    let s2 = Arc::new(MockRpc::new(10));
    let both_unknown =
        NimHtlcVerifier::new(p2, Arc::new(NimFundingStore::new())).with_secondary(s2);
    assert_eq!(
        both_unknown.observe_claim(SwapLegId::Nim, &[0x5C; 32]),
        ClaimObservation::NotFound
    );
}
