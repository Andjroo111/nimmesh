//! # polygon_verifier_tests — the offline suite for `PolygonHtlcVerifier`, extracted from
//! `polygon_verifier.rs` for the 800-line guard (a CHILD module via `#[path]`, keeping access to
//! the module's private helpers — the `polygon_gateway_tests.rs` pattern). Everything runs against
//! the [`FakeReads`] chain fake: no network in `cargo test`, ever.

use super::*;
use crate::nimiq::hex::bytes_to_hex;
use crate::swap_funding_verify::{require_funded, ConfirmationPolicy, FundingRejected};
use crate::swap_intent::Asset;
use crate::swap_leg::sha256;
use std::sync::atomic::{AtomicBool, Ordering};

const HTLC: EvmAddress = [0xDD; 20];
const US: EvmAddress = [0xAA; 20];

/// A programmable chain fake: logs + per-swap states + head, with a kill switch that makes
/// every read fail (the transport-error path).
struct FakeReads {
    logs: Vec<EvmLog>,
    // (swap_id, state) — getSwap answers from this table.
    states: Vec<([u8; 32], u64)>,
    head: u64,
    broken: AtomicBool,
}

impl FakeReads {
    fn err() -> EvmRpcError {
        EvmRpcError::Transport("fake: broken".to_string())
    }
}

impl PolygonReads for FakeReads {
    fn call(&self, _to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        // getSwap(bytes32): answer the 6-word tuple with only the state word meaningful.
        let mut id = [0u8; 32];
        id.copy_from_slice(&data[4..36]);
        let state = self
            .states
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, st)| *st)
            .unwrap_or(0);
        let mut out = vec![0u8; 6 * 32];
        out[5 * 32 + 24..].copy_from_slice(&state.to_be_bytes());
        Ok(out)
    }

    fn new_swap_logs_to(
        &self,
        _htlc: &EvmAddress,
        _recipient: &EvmAddress,
        _from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(self.logs.clone())
    }

    fn head(&self) -> Result<u64, EvmRpcError> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(Self::err());
        }
        Ok(self.head)
    }
}

fn log(swap_id: [u8; 32], amount: u64, hashlock: [u8; 32], timelock: u64, block: u64) -> EvmLog {
    let mut data = vec![0u8; 96];
    data[24..32].copy_from_slice(&amount.to_be_bytes());
    data[32..64].copy_from_slice(&hashlock);
    data[88..96].copy_from_slice(&timelock.to_be_bytes());
    EvmLog {
        topic1: swap_id,
        data,
        block_number: block,
        transaction_hash: String::new(),
    }
}

fn expectation(hashlock: [u8; 32]) -> HtlcExpectation {
    HtlcExpectation {
        leg: SwapLegId::Counterparty,
        hashlock,
        min_amount: 1_000_000,
        min_timeout: 5_000,
        recipient: US.to_vec(),
        term_anchor: 0,
    }
}

fn verifier(fake: FakeReads) -> PolygonHtlcVerifier<FakeReads> {
    PolygonHtlcVerifier::new(fake, HTLC, 0)
}

#[test]
fn word_u64_refuses_over_64_bit_words_instead_of_truncating() {
    // A word that fits u64 decodes; a word with ANY byte set above the low 8 reads None
    // (LOW / G8 M5 — never a silent truncation of a > 2^64 value).
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&12_345u64.to_be_bytes());
    assert_eq!(word_u64(&w), Some(12_345));
    w[23] = 0x01; // the byte just above the low 8 → would overflow u64
    assert_eq!(word_u64(&w), None);
    // A short word is malformed, not zero.
    assert_eq!(word_u64(&[0u8; 31]), None);
}

#[test]
fn a_newswap_log_with_an_over_u64_amount_is_skipped_not_truncated() {
    // A hostile/garbled log whose amount word overflows u64 must not be read as a small
    // amount — decode returns None, the log is skipped, and (its being the only match)
    // the observation is Absent (fail-closed).
    let h = sha256(&[20u8; 32]);
    let mut bad = log([0x0A; 32], 1_000_000, h, 9_000, 50);
    bad.data[0] = 0x01; // high byte of the amount word set
    let v = verifier(FakeReads {
        logs: vec![bad],
        states: vec![([0x0A; 32], STATE_LIVE)],
        head: 60,
        broken: AtomicBool::new(false),
    });
    assert_eq!(v.observe(&expectation(h)), FundingObservation::Absent);
}

#[test]
fn topic0_matches_the_cast_vector() {
    assert_eq!(
        bytes_to_hex(&new_swap_topic0()),
        "9a28a6867cb98cb878d32dff82f780ffdab8c2c739daeb18f416835dcf7276c6"
    );
}

#[test]
fn a_live_matching_escrow_reads_found_with_depth_and_passes_the_gate() {
    let h = sha256(&[7u8; 32]);
    let v = verifier(FakeReads {
        logs: vec![log([0x01; 32], 1_500_000, h, 9_000, 100)],
        states: vec![([0x01; 32], STATE_LIVE)],
        head: 110,
        broken: AtomicBool::new(false),
    });
    let obs = v.observe(&expectation(h));
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: 11, // 110 - 100 + 1
        }
    );
    let need = ConfirmationPolicy::testnet_defaults().required(Asset::Usdc);
    assert!(require_funded(&obs, &expectation(h), need).is_ok());
}

#[test]
fn a_reorg_that_reburies_the_escrow_shallower_is_refused_again() {
    let h = sha256(&[8u8; 32]);
    let policy = ConfirmationPolicy::testnet_defaults().required(Asset::Usdc);
    // Buried deep: passes.
    let deep = verifier(FakeReads {
        logs: vec![log([0x02; 32], 2_000_000, h, 9_000, 100)],
        states: vec![([0x02; 32], STATE_LIVE)],
        head: 100 + u64::from(policy),
        broken: AtomicBool::new(false),
    });
    assert!(require_funded(&deep.observe(&expectation(h)), &expectation(h), policy).is_ok());
    // The SAME escrow after a reorg to a shallower head: the stateless gate refuses again.
    let shallow = verifier(FakeReads {
        logs: vec![log([0x02; 32], 2_000_000, h, 9_000, 100)],
        states: vec![([0x02; 32], STATE_LIVE)],
        head: 100,
        broken: AtomicBool::new(false),
    });
    assert!(matches!(
        require_funded(&shallow.observe(&expectation(h)), &expectation(h), policy),
        Err(FundingRejected::TooShallow { have: 1, .. })
    ));
}

#[test]
fn escrows_paying_us_under_a_different_hashlock_read_mismatch() {
    let ours = sha256(&[9u8; 32]);
    let theirs = sha256(&[10u8; 32]);
    let v = verifier(FakeReads {
        logs: vec![log([0x03; 32], 1_000_000, theirs, 9_000, 50)],
        states: vec![([0x03; 32], STATE_LIVE)],
        head: 60,
        broken: AtomicBool::new(false),
    });
    assert_eq!(
        v.observe(&expectation(ours)),
        FundingObservation::Mismatch(MismatchReason::Hashlock)
    );
}

#[test]
fn a_claimed_or_refunded_slot_is_not_funding() {
    let h = sha256(&[11u8; 32]);
    for resolved in [2u64, 3u64] {
        let v = verifier(FakeReads {
            logs: vec![log([0x04; 32], 1_000_000, h, 9_000, 50)],
            states: vec![([0x04; 32], resolved)],
            head: 60,
            broken: AtomicBool::new(false),
        });
        assert_eq!(v.observe(&expectation(h)), FundingObservation::Absent);
    }
}

#[test]
fn the_deepest_live_match_wins() {
    let h = sha256(&[12u8; 32]);
    let v = verifier(FakeReads {
        logs: vec![
            log([0x05; 32], 3_000_000, h, 9_500, 80), // shallower
            log([0x06; 32], 2_000_000, h, 9_000, 40), // deepest live — wins
        ],
        states: vec![([0x05; 32], STATE_LIVE), ([0x06; 32], STATE_LIVE)],
        head: 100,
        broken: AtomicBool::new(false),
    });
    assert_eq!(
        v.observe(&expectation(h)),
        FundingObservation::Found {
            amount: 2_000_000,
            timeout: 9_000,
            confirmations: 61,
        }
    );
}

#[test]
fn a_secondary_head_disagreeing_beyond_tolerance_fails_closed() {
    // M5 cross-read: a live matching escrow the primary sees deep, but the independent
    // secondary's head disagrees far beyond HEAD_CROSS_TOLERANCE_BLOCKS → fail-closed
    // (never trust a depth two endpoints can't agree on).
    let h = sha256(&[21u8; 32]);
    let primary = FakeReads {
        logs: vec![log([0x0B; 32], 1_500_000, h, 9_000, 100)],
        states: vec![([0x0B; 32], STATE_LIVE)],
        head: 200,
        broken: AtomicBool::new(false),
    };
    let sec_bad = FakeReads {
        logs: vec![],
        states: vec![],
        head: 100, // |200 - 100| = 100 >> 12
        broken: AtomicBool::new(false),
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(sec_bad);
    assert_eq!(v.observe(&expectation(h)), FundingObservation::Absent);
}

#[test]
fn an_agreeing_secondary_within_tolerance_uses_the_conservative_head() {
    // Primary head 110, honest secondary head 105 (diff 5 ≤ 12) → the depth uses the lower
    // (conservative) head, so a primary that inflates within tolerance still can't over-count.
    let h = sha256(&[22u8; 32]);
    let primary = FakeReads {
        logs: vec![log([0x0C; 32], 2_000_000, h, 9_000, 100)],
        states: vec![([0x0C; 32], STATE_LIVE)],
        head: 110,
        broken: AtomicBool::new(false),
    };
    let sec = FakeReads {
        logs: vec![],
        states: vec![],
        head: 105,
        broken: AtomicBool::new(false),
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(sec);
    assert_eq!(
        v.observe(&expectation(h)),
        FundingObservation::Found {
            amount: 2_000_000,
            timeout: 9_000,
            confirmations: 6, // min(110,105) - 100 + 1
        }
    );
}

#[test]
fn a_secondary_whose_head_read_errors_fails_closed() {
    let h = sha256(&[23u8; 32]);
    let primary = FakeReads {
        logs: vec![log([0x0D; 32], 1_000_000, h, 9_000, 100)],
        states: vec![([0x0D; 32], STATE_LIVE)],
        head: 105,
        broken: AtomicBool::new(false),
    };
    let sec_broken = FakeReads {
        logs: vec![],
        states: vec![],
        head: 105,
        broken: AtomicBool::new(true), // every read errors
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(sec_broken);
    assert_eq!(v.observe(&expectation(h)), FundingObservation::Absent);
}

#[test]
fn transport_failures_and_foreign_legs_read_absent_fail_closed() {
    let h = sha256(&[13u8; 32]);
    let v = verifier(FakeReads {
        logs: vec![log([0x07; 32], 1_000_000, h, 9_000, 50)],
        states: vec![([0x07; 32], STATE_LIVE)],
        head: 60,
        broken: AtomicBool::new(true), // every read errors
    });
    assert_eq!(v.observe(&expectation(h)), FundingObservation::Absent);

    // The NIM leg is not this verifier's chain.
    let healthy = verifier(FakeReads {
        logs: vec![log([0x08; 32], 1_000_000, h, 9_000, 50)],
        states: vec![([0x08; 32], STATE_LIVE)],
        head: 60,
        broken: AtomicBool::new(false),
    });
    let mut nim = expectation(h);
    nim.leg = SwapLegId::Nim;
    assert_eq!(healthy.observe(&nim), FundingObservation::Absent);

    // A malformed own-recipient can never be paid: Absent, never an advance.
    let mut bad = expectation(h);
    bad.recipient = vec![0xAA; 19];
    assert_eq!(healthy.observe(&bad), FundingObservation::Absent);
}

// --- the finalized fast-path (ADR-0003 addendum, 2026-07-15) --------------------------------------

/// [`FakeReads`] plus a programmable `finalized` tag (`None` = the endpoint does not serve it —
/// the trait default's error, i.e. the depth-count fallback).
struct FinalizedReads {
    inner: FakeReads,
    finalized: Option<u64>,
}

impl PolygonReads for FinalizedReads {
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        self.inner.call(to, data)
    }
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        self.inner.new_swap_logs_to(htlc, recipient, from_block)
    }
    fn head(&self) -> Result<u64, EvmRpcError> {
        self.inner.head()
    }
    fn finalized_head(&self) -> Result<u64, EvmRpcError> {
        self.finalized.ok_or_else(FakeReads::err)
    }
}

/// A live escrow for hashlock-of-`seed` at `block`, on a chain whose head is `head` and whose
/// `finalized` tag reads `finalized` (`None` = unserved).
fn finalized_rig(seed: u8, block: u64, head: u64, finalized: Option<u64>) -> FinalizedReads {
    let h = sha256(&[seed; 32]);
    FinalizedReads {
        inner: FakeReads {
            logs: vec![log([seed; 32], 1_500_000, h, 9_000, block)],
            states: vec![([seed; 32], STATE_LIVE)],
            head,
            broken: AtomicBool::new(false),
        },
        finalized,
    }
}

#[test]
fn a_finalized_escrow_reads_maximally_buried_and_clears_the_mainnet_gate() {
    // Escrow at block 100, head 105 (raw depth 6), finalized 102 ≥ the inclusion block →
    // deterministic finality reports FINALIZED_CONFIRMATIONS, clearing the mainnet USDC floor
    // (8) that the raw count alone would still refuse. This is the quantum the fast-finality
    // profile removes: ~5 s to the milestone instead of a multi-block wait.
    let h = sha256(&[31u8; 32]);
    let v = PolygonHtlcVerifier::new(finalized_rig(31, 100, 105, Some(102)), HTLC, 0);
    let obs = v.observe(&expectation(h));
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: FINALIZED_CONFIRMATIONS,
        }
    );
    let need = ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc);
    assert!(require_funded(&obs, &expectation(h), need).is_ok());
    // Even the paranoid profile's 64-deep floor clears — finality outranks any count.
    let paranoid = ConfirmationPolicy::mainnet_paranoid().required(Asset::Usdc);
    assert!(require_funded(&obs, &expectation(h), paranoid).is_ok());
}

#[test]
fn an_escrow_above_the_finalized_height_keeps_depth_counting() {
    // finalized 99 < inclusion block 100 → the fast path must NOT fire; the observation is
    // today's raw depth (6), which the mainnet floor (8) still refuses — fail-closed.
    let h = sha256(&[32u8; 32]);
    let v = PolygonHtlcVerifier::new(finalized_rig(32, 100, 105, Some(99)), HTLC, 0);
    let obs = v.observe(&expectation(h));
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: 6,
        }
    );
    let need = ConfirmationPolicy::mainnet_defaults().required(Asset::Usdc);
    assert!(matches!(
        require_funded(&obs, &expectation(h), need),
        Err(FundingRejected::TooShallow { have: 6, need: 8 })
    ));
}

#[test]
fn a_missing_finalized_tag_falls_back_to_depth_counting() {
    // The endpoint does not serve the tag (the trait default / an RPC error): the observation
    // is EXACTLY the pre-finality depth count — the fallback can only be slower, never weaker.
    let h = sha256(&[33u8; 32]);
    let v = PolygonHtlcVerifier::new(finalized_rig(33, 100, 120, None), HTLC, 0);
    assert_eq!(
        v.observe(&expectation(h)),
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: 21, // 120 - 100 + 1
        }
    );
}

#[test]
fn a_lying_primary_finalized_claim_is_capped_by_the_secondary() {
    // M5 lying-RPC posture on the finalized path: the primary claims the escrow's block is
    // long finalized (200); the independent secondary says finality is only at 90 — the min
    // wins, the escrow (block 100) is NOT finalized, and the raw depth (11 < 64 under the
    // paranoid floor used here as the strict gate) does not authorize. A single lying/MITM'd
    // endpoint can no longer fake finality.
    let h = sha256(&[34u8; 32]);
    let primary = finalized_rig(34, 100, 110, Some(200));
    let secondary = FinalizedReads {
        inner: FakeReads {
            logs: vec![],
            states: vec![],
            head: 110,
            broken: AtomicBool::new(false),
        },
        finalized: Some(90),
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(secondary);
    let obs = v.observe(&expectation(h));
    assert_eq!(
        obs,
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: 11, // depth count off the conservative head — NOT finalized
        }
    );
    let strict = ConfirmationPolicy::mainnet_paranoid().required(Asset::Usdc);
    assert!(matches!(
        require_funded(&obs, &expectation(h), strict),
        Err(FundingRejected::TooShallow { have: 11, need: 64 })
    ));
}

#[test]
fn a_secondary_without_the_finalized_tag_blocks_the_finality_fast_path() {
    // A wired secondary that cannot vouch (tag unserved / read error) must not let the
    // primary's finalized claim authorize alone — depth counting holds. Same escrow as the
    // happy path (block 100, head 105, primary finalized 102), only the secondary differs.
    let h = sha256(&[35u8; 32]);
    let primary = finalized_rig(35, 100, 105, Some(102));
    let secondary = FinalizedReads {
        inner: FakeReads {
            logs: vec![],
            states: vec![],
            head: 105,
            broken: AtomicBool::new(false),
        },
        finalized: None,
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(secondary);
    assert_eq!(
        v.observe(&expectation(h)),
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: 6, // depth count — finality did not fire
        }
    );
}

#[test]
fn an_agreeing_secondary_finalizes_conservatively() {
    // Both endpoints serve the tag: the min drives it. Escrow at block 100; primary finalized
    // 104, secondary 101 → min 101 ≥ 100 → finalized. And the claim never exceeds the
    // cross-checked head (the min(head) cap in conservative_finalized).
    let h = sha256(&[36u8; 32]);
    let primary = finalized_rig(36, 100, 105, Some(104));
    let secondary = FinalizedReads {
        inner: FakeReads {
            logs: vec![],
            states: vec![],
            head: 103,
            broken: AtomicBool::new(false),
        },
        finalized: Some(101),
    };
    let v = PolygonHtlcVerifier::new(primary, HTLC, 0).with_secondary(secondary);
    assert_eq!(
        v.observe(&expectation(h)),
        FundingObservation::Found {
            amount: 1_500_000,
            timeout: 9_000,
            confirmations: FINALIZED_CONFIRMATIONS,
        }
    );
}

#[test]
fn conservative_finalized_takes_the_min_and_fails_closed() {
    let reads = |fin: Option<u64>| FinalizedReads {
        inner: FakeReads {
            logs: vec![],
            states: vec![],
            head: 100,
            broken: AtomicBool::new(false),
        },
        finalized: fin,
    };
    // Primary alone: its claim, capped at the cross-checked head.
    assert_eq!(
        conservative_finalized(&reads(Some(90)), None, 100),
        Some(90)
    );
    assert_eq!(
        conservative_finalized(&reads(Some(500)), None, 100),
        Some(100) // never beyond the head both endpoints agreed on
    );
    // Primary unserved → no finality, regardless of the secondary.
    assert_eq!(conservative_finalized(&reads(None), None, 100), None);
    assert_eq!(
        conservative_finalized(&reads(None), Some(&reads(Some(90))), 100),
        None
    );
    // Secondary wired: BOTH must vouch; the min wins.
    assert_eq!(
        conservative_finalized(&reads(Some(90)), Some(&reads(Some(80))), 100),
        Some(80)
    );
    assert_eq!(
        conservative_finalized(&reads(Some(80)), Some(&reads(Some(90))), 100),
        Some(80)
    );
    assert_eq!(
        conservative_finalized(&reads(Some(90)), Some(&reads(None)), 100),
        None
    );
}
