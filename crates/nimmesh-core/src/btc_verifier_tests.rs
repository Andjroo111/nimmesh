//! Offline tests for [`BtcHtlcVerifier`] against a fake [`BitcoinReads`] seam — no network, no
//! `bitcoin` crate (the P2WSH derivation is proven separately in the `bitcoin-leg` test at the
//! bottom). Covers the FundingVerifier contract: found-at-depth, too-shallow, wrong-script,
//! spent-output, transport-error, and the M5 cross-read (disagree → fail-closed, agree → conservative).

use super::*;
use crate::swap_funding_verify::{require_funded, ConfirmationPolicy, FundingRejected};
use crate::swap_intent::Asset;
use std::sync::atomic::{AtomicBool, Ordering};

const HASHLOCK: [u8; 32] = [0x7E; 32];
/// A cross-chain CLTV is a Unix-SECONDS timestamp (≥ 500_000_000).
const CLTV: u64 = 1_800_000_000;
const ADDR: &str = "tb1qhtlc-example-p2wsh-address";

/// A P2WSH scriptPubKey `OP_0 <32-byte program>` with a chosen program filler byte.
fn p2wsh(program_byte: u8) -> Vec<u8> {
    let mut spk = vec![0x00, 0x20];
    spk.extend_from_slice(&[program_byte; 32]);
    spk
}
/// The watched HTLC scriptPubKey the verifier is built with.
fn expected_spk() -> Vec<u8> {
    p2wsh(0xAB)
}
/// A P2WPKH `OP_0 <20-byte program>` — a plausible change/payout output (never our HTLC script).
fn p2wpkh() -> Vec<u8> {
    let mut spk = vec![0x00, 0x14];
    spk.extend_from_slice(&[0xCC; 20]);
    spk
}

fn expectation() -> HtlcExpectation {
    HtlcExpectation {
        leg: SwapLegId::Counterparty,
        hashlock: HASHLOCK,
        min_amount: 20_000,
        min_timeout: 1_700_000_000, // ≤ CLTV, so the timeout floor passes
        recipient: Vec::new(),      // the BTC leg binds via the P2WSH; recipient is unused here
        term_anchor: 0,
    }
}

/// A programmable reads fake: address txs + per-txid status + tip, with a kill switch making every
/// read fail (the transport-error path).
struct FakeReads {
    txs: Vec<BtcAddressTx>,
    statuses: Vec<(String, BtcTxStatus)>,
    tip: u64,
    broken: AtomicBool,
}

impl FakeReads {
    fn ok(txs: Vec<BtcAddressTx>, statuses: Vec<(&str, BtcTxStatus)>, tip: u64) -> Self {
        FakeReads {
            txs,
            statuses: statuses
                .into_iter()
                .map(|(t, s)| (t.to_string(), s))
                .collect(),
            tip,
            broken: AtomicBool::new(false),
        }
    }
    fn err() -> BtcReadsError {
        BtcReadsError::Transport {
            reason: "fake: broken".into(),
        }
    }
}

impl BitcoinReads for FakeReads {
    fn address_txs(&self, _address: &str) -> Result<Vec<BtcAddressTx>, BtcReadsError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(self.txs.clone())
    }
    fn tx_status(&self, txid: &str) -> Result<BtcTxStatus, BtcReadsError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(self
            .statuses
            .iter()
            .find(|(t, _)| t == txid)
            .map(|(_, s)| s.clone())
            .unwrap_or(BtcTxStatus {
                confirmed: false,
                block_height: None,
            }))
    }
    fn tip_height(&self) -> Result<u64, BtcReadsError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(self.tip)
    }
}

fn confirmed(h: u64) -> BtcTxStatus {
    BtcTxStatus {
        confirmed: true,
        block_height: Some(h),
    }
}
fn fund_tx(txid: &str, spk: Vec<u8>, value: u64) -> BtcAddressTx {
    BtcAddressTx {
        txid: txid.into(),
        spends: Vec::new(),
        outputs: vec![BtcTxOut {
            scriptpubkey: spk,
            value_sat: value,
        }],
    }
}
fn spend_tx(txid: &str, funding_txid: &str, funding_vout: u32) -> BtcAddressTx {
    BtcAddressTx {
        txid: txid.into(),
        spends: vec![BtcOutpoint {
            txid: funding_txid.into(),
            vout: funding_vout,
        }],
        outputs: vec![BtcTxOut {
            scriptpubkey: p2wpkh(),
            value_sat: 19_000,
        }],
    }
}
fn verifier(fake: FakeReads) -> BtcHtlcVerifier<FakeReads> {
    BtcHtlcVerifier::new(fake, expected_spk(), ADDR.into(), HASHLOCK, CLTV)
}
fn btc_depth() -> u32 {
    ConfirmationPolicy::testnet_defaults().required(Asset::Btc) // 3
}

#[test]
fn a_funded_htlc_at_depth_reads_found_and_passes_the_gate() {
    let v = verifier(FakeReads::ok(
        vec![fund_tx("f1", expected_spk(), 25_000)],
        vec![("f1", confirmed(100))],
        105,
    ));
    let obs = v.observe(&expectation());
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 25_000,
            timeout: CLTV,
            confirmations: 6, // 105 - 100 + 1
        }
    );
    assert!(require_funded(&obs, &expectation(), btc_depth()).is_ok());
}

#[test]
fn a_change_output_alongside_the_htlc_does_not_confuse_detection() {
    // A real funding tx pays the P2WSH HTLC AND a change output back to the funder — the change
    // (P2WPKH) must neither be mistaken for funding nor flagged as a wrong script.
    let mut tx = fund_tx("f1b", expected_spk(), 25_000);
    tx.outputs.push(BtcTxOut {
        scriptpubkey: p2wpkh(),
        value_sat: 500_000,
    });
    let v = verifier(FakeReads::ok(vec![tx], vec![("f1b", confirmed(100))], 110));
    assert_eq!(
        v.observe(&expectation()),
        FundingObservation::Found {
            amount: 25_000,
            timeout: CLTV,
            confirmations: 11,
        }
    );
}

#[test]
fn a_shallow_funding_is_reported_shallow_and_the_gate_refuses_it() {
    let v = verifier(FakeReads::ok(
        vec![fund_tx("f2", expected_spk(), 25_000)],
        vec![("f2", confirmed(100))],
        101, // depth = 2 < 3
    ));
    let obs = v.observe(&expectation());
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 25_000,
            timeout: CLTV,
            confirmations: 2,
        }
    );
    assert!(matches!(
        require_funded(&obs, &expectation(), btc_depth()),
        Err(FundingRejected::TooShallow { have: 2, need: 3 })
    ));
}

#[test]
fn an_unconfirmed_mempool_funding_reads_zero_confirmations() {
    // Present but still in the mempool → 0 confirmations; the gate refuses it as too shallow.
    let v = verifier(FakeReads::ok(
        vec![fund_tx("f2b", expected_spk(), 25_000)],
        vec![(
            "f2b",
            BtcTxStatus {
                confirmed: false,
                block_height: None,
            },
        )],
        200,
    ));
    let obs = v.observe(&expectation());
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 25_000,
            timeout: CLTV,
            confirmations: 0,
        }
    );
    assert!(matches!(
        require_funded(&obs, &expectation(), btc_depth()),
        Err(FundingRejected::TooShallow { have: 0, need: 3 })
    ));
}

#[test]
fn an_underfunded_htlc_reads_found_and_the_gate_flags_underfunded() {
    // The exact script, but locked less than agreed — reported (not hidden), the gate says why.
    let v = verifier(FakeReads::ok(
        vec![fund_tx("uf", expected_spk(), 10_000)], // < 20_000 min
        vec![("uf", confirmed(100))],
        200,
    ));
    let obs = v.observe(&expectation());
    assert!(matches!(
        obs,
        FundingObservation::Found { amount: 10_000, .. }
    ));
    assert!(matches!(
        require_funded(&obs, &expectation(), btc_depth()),
        Err(FundingRejected::Underfunded {
            have: 10_000,
            need: 20_000
        })
    ));
}

#[test]
fn a_p2wsh_output_that_is_not_our_exact_script_reads_mismatch() {
    // A different 32-byte witness program → a different P2WSH → not our HTLC. Fail-closed to
    // Mismatch (something at our address commits to terms we did not agree), never a silent Found.
    let v = verifier(FakeReads::ok(
        vec![fund_tx("w1", p2wsh(0x99), 25_000)],
        vec![("w1", confirmed(100))],
        200,
    ));
    assert_eq!(
        v.observe(&expectation()),
        FundingObservation::Mismatch(MismatchReason::Recipient)
    );
}

#[test]
fn a_spent_funding_output_reads_absent_resolved_is_not_funding() {
    // The HTLC was funded (f3) then claimed/refunded (s3 spends f3:0) — a resolved slot is not
    // funding, mirroring the NIM/Polygon verifiers.
    let v = verifier(FakeReads::ok(
        vec![
            fund_tx("f3", expected_spk(), 25_000),
            spend_tx("s3", "f3", 0),
        ],
        vec![("f3", confirmed(100)), ("s3", confirmed(150))],
        200,
    ));
    assert_eq!(v.observe(&expectation()), FundingObservation::Absent);
}

#[test]
fn transport_errors_and_foreign_legs_and_wrong_swaps_read_absent_fail_closed() {
    // Every read errors → Absent (a transport blip can delay a swap, never authorize one).
    let broken = FakeReads {
        txs: vec![fund_tx("f4", expected_spk(), 25_000)],
        statuses: vec![("f4".to_string(), confirmed(100))],
        tip: 200,
        broken: AtomicBool::new(true),
    };
    assert_eq!(
        verifier(broken).observe(&expectation()),
        FundingObservation::Absent
    );

    // The NIM leg is not this verifier's chain.
    let mut nim = expectation();
    nim.leg = SwapLegId::Nim;
    let healthy = verifier(FakeReads::ok(
        vec![fund_tx("f5", expected_spk(), 25_000)],
        vec![("f5", confirmed(100))],
        200,
    ));
    assert_eq!(healthy.observe(&nim), FundingObservation::Absent);

    // An expectation for a DIFFERENT hashlock is not this verifier's swap.
    let mut other = expectation();
    other.hashlock = [0x11; 32];
    let healthy2 = verifier(FakeReads::ok(
        vec![fund_tx("f5b", expected_spk(), 25_000)],
        vec![("f5b", confirmed(100))],
        200,
    ));
    assert_eq!(healthy2.observe(&other), FundingObservation::Absent);
}

#[test]
fn nothing_on_chain_reads_absent() {
    let v = verifier(FakeReads::ok(vec![], vec![], 200));
    assert_eq!(v.observe(&expectation()), FundingObservation::Absent);
}

#[test]
fn a_secondary_disagreeing_on_the_funding_block_fails_closed() {
    // M5 cross-read: the primary sees f6 at block 100, but the independent secondary reports a
    // DIFFERENT height → never trust a depth two sources can't agree on.
    let primary = FakeReads::ok(
        vec![fund_tx("f6", expected_spk(), 25_000)],
        vec![("f6", confirmed(100))],
        200,
    );
    let sec_bad = FakeReads::ok(vec![], vec![("f6", confirmed(105))], 200); // 105 != 100
    let v = BtcHtlcVerifier::new(primary, expected_spk(), ADDR.into(), HASHLOCK, CLTV)
        .with_secondary(sec_bad);
    assert_eq!(v.observe(&expectation()), FundingObservation::Absent);
}

#[test]
fn a_secondary_that_has_not_seen_the_tx_fails_closed() {
    let primary = FakeReads::ok(
        vec![fund_tx("f6b", expected_spk(), 25_000)],
        vec![("f6b", confirmed(100))],
        200,
    );
    let sec_blind = FakeReads::ok(vec![], vec![], 200); // no status for f6b → unconfirmed → disagree
    let v = BtcHtlcVerifier::new(primary, expected_spk(), ADDR.into(), HASHLOCK, CLTV)
        .with_secondary(sec_blind);
    assert_eq!(v.observe(&expectation()), FundingObservation::Absent);
}

#[test]
fn an_agreeing_secondary_uses_the_conservative_tip() {
    // Both agree f7 is at block 100; primary tip 130, honest secondary tip 120 → the depth uses
    // the lower (conservative) tip, so a primary that inflates its tip still can't over-count.
    let primary = FakeReads::ok(
        vec![fund_tx("f7", expected_spk(), 25_000)],
        vec![("f7", confirmed(100))],
        130,
    );
    let sec = FakeReads::ok(vec![], vec![("f7", confirmed(100))], 120);
    let v = BtcHtlcVerifier::new(primary, expected_spk(), ADDR.into(), HASHLOCK, CLTV)
        .with_secondary(sec);
    assert_eq!(
        v.observe(&expectation()),
        FundingObservation::Found {
            amount: 25_000,
            timeout: CLTV,
            confirmations: 21, // min(130, 120) - 100 + 1
        }
    );
}

#[test]
fn a_secondary_whose_tip_read_errors_fails_closed() {
    let primary = FakeReads::ok(
        vec![fund_tx("f8", expected_spk(), 25_000)],
        vec![("f8", confirmed(100))],
        120,
    );
    let sec_broken = FakeReads {
        txs: vec![],
        statuses: vec![("f8".to_string(), confirmed(100))],
        tip: 120,
        broken: AtomicBool::new(true),
    };
    let v = BtcHtlcVerifier::new(primary, expected_spk(), ADDR.into(), HASHLOCK, CLTV)
        .with_secondary(sec_broken);
    assert_eq!(v.observe(&expectation()), FundingObservation::Absent);
}

#[test]
fn is_p2wsh_recognizes_only_the_witness_v0_script_hash_shape() {
    assert!(is_p2wsh(&p2wsh(0x01)));
    assert!(!is_p2wsh(&p2wpkh())); // 22-byte P2WPKH
    assert!(!is_p2wsh(
        &[0x51, 0x20][..]
            .iter()
            .chain([0u8; 32].iter())
            .copied()
            .collect::<Vec<_>>()
    )); // P2TR
    assert!(!is_p2wsh(&[0x00, 0x20])); // truncated
}

// The bitcoin-dependent half: `from_params` derives EXACTLY the P2WSH scriptPubKey + address the
// real chain would show for the agreed terms (proven only with the `bitcoin-leg` feature).
#[cfg(feature = "bitcoin-leg")]
#[test]
fn from_params_derives_the_canonical_p2wsh_the_chain_shows() {
    use crate::btc::BtcHtlcParams;
    use bitcoin::Network;

    // The proven reference params (btc::tests / swap_btc_leg::tests).
    let params = BtcHtlcParams {
        hash_root: crate::nimiq::hex::hex_to_bytes(
            "ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c9",
        )
        .unwrap()
        .try_into()
        .unwrap(),
        recipient_pubkey: crate::nimiq::hex::hex_to_bytes(
            "034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa",
        )
        .unwrap()
        .try_into()
        .unwrap(),
        sender_pubkey: crate::nimiq::hex::hex_to_bytes(
            "02466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27",
        )
        .unwrap()
        .try_into()
        .unwrap(),
        cltv_locktime: 1_782_588_246,
    };
    let expected_spk = params.script_pubkey(Network::Signet).as_bytes().to_vec();
    let expected_addr = params.p2wsh_address(Network::Signet).to_string();

    // A funding output at that exact script, seen deep, verifies through `from_params`.
    let reads = FakeReads::ok(
        vec![fund_tx("cf", expected_spk.clone(), 100_000)],
        vec![("cf", confirmed(1000))],
        1010,
    );
    let v = BtcHtlcVerifier::from_params(reads, &params, Network::Signet);
    let expect = HtlcExpectation {
        leg: SwapLegId::Counterparty,
        hashlock: params.hash_root,
        min_amount: 100_000,
        min_timeout: 1_782_588_246,
        recipient: Vec::new(),
        term_anchor: 0,
    };
    assert_eq!(
        v.observe(&expect),
        FundingObservation::Found {
            amount: 100_000,
            timeout: 1_782_588_246,
            confirmations: 11,
        }
    );
    assert!(require_funded(&v.observe(&expect), &expect, btc_depth()).is_ok());
    // The derived P2WSH matches the address both sides fund (starts tb1q on signet).
    assert!(expected_addr.starts_with("tb1q"));
}
