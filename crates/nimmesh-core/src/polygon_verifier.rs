//! # polygon_verifier — the gateway-backed USDC-leg [`FundingVerifier`] (#72 tail, slice 1; behind `polygon-gateway`)
//!
//! The REAL-chain implementation of the G1 funding-verification seam for the Polygon leg: before
//! this node funds or reveals, [`PolygonHtlcVerifier`] checks the **deployed** `NimmeshHtlc`
//! (`docs/swap/AMOY.md`) for a live escrow paying OUR recipient under OUR hashlock — mirroring
//! [`crate::swap_funding_verify::LedgerVerifier`]'s reference semantics against actual chain state:
//!
//! 1. `eth_getLogs` for `NewSwap` events on the HTLC **indexed by our recipient** (topic 3);
//! 2. among those, a log whose `hashlock` (data word 2) matches the expectation is a candidate;
//!    `getSwap(swapId)` (`eth_call`) must still read **Live** — a claimed/refunded slot is not
//!    funding. The DEEPEST live candidate wins (lowest block, like `LedgerVerifier`);
//! 3. depth = `eth_blockNumber` − the log's block + 1 — feeding [`require_funded`]'s
//!    per-chain [`ConfirmationPolicy`](crate::swap_funding_verify::ConfirmationPolicy) floor
//!    (#74/G3): a reorg that re-buries the escrow shallower is refused again on the next
//!    observation, because the gate re-runs statelessly every time.
//!
//! **Fail-closed:** any RPC failure reads as [`FundingObservation::Absent`] — the gate then
//! refuses to advance (`NotFundedYet`) and simply retries later. A transport blip can delay a
//! swap; it can never authorize one.
//!
//! **Mismatch semantics** (LedgerVerifier mirror, one documented divergence): escrows paying us
//! under a DIFFERENT hashlock read as `Mismatch(Hashlock)`; an escrow under our hashlock paying
//! someone else is NOT discoverable here (the log query is recipient-indexed) and reads as
//! `Absent` — same refusal, different label, still safe.
//!
//! Logic is tested offline against a [`PolygonReads`] fake; the codec halves live in
//! [`crate::polygon_gateway`] with their own fixture tests. The LIVE wiring (this verifier over
//! [`HttpPolygonRpc`] against Amoy) is the #72-tail proof that rides the G6 deployment.

use crate::evm::{function_selector, keccak256};
use crate::polygon_gateway::{EvmLog, EvmRpcError, HttpPolygonRpc};
use crate::swap_funding_verify::{
    FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_usdc_leg::EvmAddress;
use crate::swap_wire::SwapLegId;

/// `keccak256("NewSwap(bytes32,address,address,uint256,bytes32,uint256)")` — the `NewSwap`
/// event's topic 0 (asserted against the cast-derived constant in the tests).
pub fn new_swap_topic0() -> [u8; 32] {
    keccak256(b"NewSwap(bytes32,address,address,uint256,bytes32,uint256)")
}

/// The chain reads this verifier needs — a seam so the logic tests offline. The live
/// implementation is [`HttpPolygonRpc`].
pub trait PolygonReads {
    /// A read-only contract call, decoded result bytes.
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError>;
    /// `NewSwap` logs on `htlc` whose indexed receiver (topic 3) is `recipient`.
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError>;
    /// The current head height.
    fn head(&self) -> Result<u64, EvmRpcError>;
}

fn addr_hex(a: &EvmAddress) -> String {
    format!("0x{}", crate::nimiq::hex::bytes_to_hex(a))
}

fn topic_hex(word: &[u8; 32]) -> String {
    format!("0x{}", crate::nimiq::hex::bytes_to_hex(word))
}

/// The indexed-receiver topic: the 20-byte address left-padded to a 32-byte word (how the EVM
/// stores an indexed `address`).
fn recipient_topic(recipient: &EvmAddress) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(recipient);
    w
}

impl PolygonReads for HttpPolygonRpc {
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        let out = self.eth_call(
            &addr_hex(to),
            &format!("0x{}", crate::nimiq::hex::bytes_to_hex(data)),
        )?;
        crate::nimiq::hex::hex_to_bytes(&out).map_err(|_| EvmRpcError::BadResponse {
            method: "eth_call".to_string(),
        })
    }

    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        self.get_logs(
            &addr_hex(htlc),
            &topic_hex(&new_swap_topic0()),
            Some(&topic_hex(&recipient_topic(recipient))),
            from_block,
        )
    }

    fn head(&self) -> Result<u64, EvmRpcError> {
        self.block_number()
    }
}

/// `getSwap(bytes32)` calldata (the verifier's live-state read).
fn get_swap_calldata(swap_id: &[u8; 32]) -> Vec<u8> {
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(swap_id);
    cd
}

/// The low 8 bytes of a 32-byte ABI word (amounts/times/states all fit `u64` in our domain).
fn word_u64(word: &[u8]) -> u64 {
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[24..32]);
    u64::from_be_bytes(be)
}

/// The `NewSwap` data words: `(amount, hashlock, timelock)`. `None` if the payload isn't the
/// event's exact 3-word shape.
fn decode_new_swap_data(data: &[u8]) -> Option<(u64, [u8; 32], u64)> {
    if data.len() != 96 {
        return None;
    }
    let mut hashlock = [0u8; 32];
    hashlock.copy_from_slice(&data[32..64]);
    Some((word_u64(&data[..32]), hashlock, word_u64(&data[64..96])))
}

/// The `getSwap` return slice the verifier needs: `(state, )` — state word 6 of 6.
/// `None` on a malformed response.
fn decode_get_swap_state(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 6 * 32 {
        return None;
    }
    Some(word_u64(&bytes[5 * 32..6 * 32]))
}

/// `NimmeshHtlc.State.Live` — the only state that counts as funding.
const STATE_LIVE: u64 = 1;

/// The gateway-backed USDC-leg funding verifier. Construct with the chain reads (live:
/// [`HttpPolygonRpc`]), the deployed HTLC address, and the earliest block worth scanning
/// (the contract's deployment block — earlier logs cannot exist).
pub struct PolygonHtlcVerifier<R: PolygonReads> {
    reads: R,
    htlc: EvmAddress,
    from_block: u64,
}

impl<R: PolygonReads> PolygonHtlcVerifier<R> {
    /// A verifier over `reads` for the `NimmeshHtlc` at `htlc`, scanning logs from `from_block`.
    pub fn new(reads: R, htlc: EvmAddress, from_block: u64) -> Self {
        PolygonHtlcVerifier {
            reads,
            htlc,
            from_block,
        }
    }

    fn observe_usdc(&self, expect: &HtlcExpectation) -> FundingObservation {
        // Our own claim address must be a 20-byte EVM address; a malformed expectation can never
        // be paid, so it reads Absent (fail-closed) rather than advancing anything.
        let recipient: EvmAddress = match expect.recipient.as_slice().try_into() {
            Ok(r) => r,
            Err(_) => return FundingObservation::Absent,
        };
        let logs = match self
            .reads
            .new_swap_logs_to(&self.htlc, &recipient, self.from_block)
        {
            Ok(l) => l,
            Err(_) => return FundingObservation::Absent, // fail-closed on transport
        };
        let head = match self.reads.head() {
            Ok(h) => h,
            Err(_) => return FundingObservation::Absent,
        };

        let mut other_hashlock_pays_us = false;
        let mut best: Option<(u64, u64, u64)> = None; // (block, amount, timeout)
        for log in &logs {
            let Some((amount, hashlock, timelock)) = decode_new_swap_data(&log.data) else {
                continue;
            };
            if hashlock != expect.hashlock {
                other_hashlock_pays_us = true;
                continue;
            }
            // The escrow must STILL be live — a claimed/refunded slot is not funding.
            let Ok(bytes) = self.reads.call(&self.htlc, &get_swap_calldata(&log.topic1)) else {
                return FundingObservation::Absent; // fail-closed on transport
            };
            match decode_get_swap_state(&bytes) {
                Some(STATE_LIVE) => {}
                _ => continue,
            }
            // Deepest live candidate wins (lowest block), mirroring LedgerVerifier.
            if best.map_or(true, |(b, _, _)| log.block_number < b) {
                best = Some((log.block_number, amount, timelock));
            }
        }

        match best {
            Some((block, amount, timeout)) => {
                let depth64 = head.saturating_sub(block).saturating_add(1);
                let confirmations = u32::try_from(depth64).unwrap_or(u32::MAX);
                FundingObservation::Found {
                    amount,
                    timeout,
                    confirmations,
                }
            }
            None if other_hashlock_pays_us => {
                FundingObservation::Mismatch(MismatchReason::Hashlock)
            }
            None => FundingObservation::Absent,
        }
    }
}

impl<R: PolygonReads + Send + Sync> FundingVerifier for PolygonHtlcVerifier<R> {
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation {
        // This verifier speaks only the counterparty (USDC) leg; the NIM leg has its own gateway.
        if expect.leg != SwapLegId::Counterparty {
            return FundingObservation::Absent;
        }
        self.observe_usdc(expect)
    }
}

#[cfg(test)]
mod tests {
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

    fn log(
        swap_id: [u8; 32],
        amount: u64,
        hashlock: [u8; 32],
        timelock: u64,
        block: u64,
    ) -> EvmLog {
        let mut data = vec![0u8; 96];
        data[24..32].copy_from_slice(&amount.to_be_bytes());
        data[32..64].copy_from_slice(&hashlock);
        data[88..96].copy_from_slice(&timelock.to_be_bytes());
        EvmLog {
            topic1: swap_id,
            data,
            block_number: block,
        }
    }

    fn expectation(hashlock: [u8; 32]) -> HtlcExpectation {
        HtlcExpectation {
            leg: SwapLegId::Counterparty,
            hashlock,
            min_amount: 1_000_000,
            min_timeout: 5_000,
            recipient: US.to_vec(),
        }
    }

    fn verifier(fake: FakeReads) -> PolygonHtlcVerifier<FakeReads> {
        PolygonHtlcVerifier::new(fake, HTLC, 0)
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
}
