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
//!    observation, because the gate re-runs statelessly every time. **Finalized fast-path
//!    (ADR-0003 addendum, 2026-07-15):** when the escrow's inclusion block is at or below the
//!    chain's `finalized` tag (Heimdall v2 milestone finality, ~5 s), the depth is reported as
//!    [`FINALIZED_CONFIRMATIONS`] — deterministic finality is strictly stronger than any count.
//!    A finalized-read failure (or a disagreeing/absent secondary) just falls back to the
//!    depth count above — slower, never less safe.
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
    FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason, FINALIZED_CONFIRMATIONS,
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
    /// The highest DETERMINISTICALLY-final block height (the `finalized` tag — Polygon PoS:
    /// Heimdall v2 milestone finality, ~5 s). The default errors, meaning "tag not served" —
    /// the verifier then keeps plain depth counting (strictly slower, never less safe), so a
    /// fake/endpoint that predates the tag changes nothing. [`HttpPolygonRpc`] overrides it.
    fn finalized_head(&self) -> Result<u64, EvmRpcError> {
        Err(EvmRpcError::BadResponse {
            method: "eth_getBlockByNumber(finalized)".to_string(),
        })
    }
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

/// The finalized height BOTH endpoints vouch for (ADR-0003 addendum, 2026-07-15): the primary's
/// `finalized`-tag read, capped by the secondary's when one is wired (the **min** of the two — a
/// lying primary cannot inflate finality past an honest secondary, the M5 lying-RPC posture
/// extended to the finalized path), and never beyond the already-cross-checked `head`. `None`
/// whenever any required read fails — an absent tag or an erroring secondary must NOT let
/// finality authorize; the caller then keeps plain depth counting (slower, never less safe).
pub(crate) fn conservative_finalized<R: PolygonReads>(
    primary: &R,
    secondary: Option<&R>,
    head: u64,
) -> Option<u64> {
    let fp = primary.finalized_head().ok()?;
    let fin = match secondary {
        None => fp,
        Some(sec) => fp.min(sec.finalized_head().ok()?),
    };
    Some(fin.min(head))
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

    fn finalized_head(&self) -> Result<u64, EvmRpcError> {
        self.finalized_block_number()
    }
}

/// `getSwap(bytes32)` calldata (the verifier's live-state read).
fn get_swap_calldata(swap_id: &[u8; 32]) -> Vec<u8> {
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(swap_id);
    cd
}

/// The low 8 bytes of a 32-byte ABI word (amounts/times/states all fit `u64` in our domain).
///
/// LOW (G8 M5): a well-formed word in our domain has its high **24 bytes zero**. A word whose
/// value would not fit `u64` is a malformed or hostile response — it reads `None` (fail-closed,
/// the log/state is skipped) rather than SILENTLY TRUNCATING to a plausible-looking low-64-bit
/// number. Defense-in-depth: a `> 2^64` amount/timelock/state must never masquerade as a small
/// one.
fn word_u64(word: &[u8]) -> Option<u64> {
    if word.len() != 32 || word[..24].iter().any(|&b| b != 0) {
        return None;
    }
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[24..32]);
    Some(u64::from_be_bytes(be))
}

/// The `NewSwap` data words: `(amount, hashlock, timelock)`. `None` if the payload isn't the
/// event's exact 3-word shape (or any numeric word would overflow `u64`).
fn decode_new_swap_data(data: &[u8]) -> Option<(u64, [u8; 32], u64)> {
    if data.len() != 96 {
        return None;
    }
    let mut hashlock = [0u8; 32];
    hashlock.copy_from_slice(&data[32..64]);
    Some((word_u64(&data[..32])?, hashlock, word_u64(&data[64..96])?))
}

/// The `getSwap` return slice the verifier needs: `(state, )` — state word 6 of 6.
/// `None` on a malformed response (or an over-`u64` state word).
fn decode_get_swap_state(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 6 * 32 {
        return None;
    }
    word_u64(&bytes[5 * 32..6 * 32])
}

/// `NimmeshHtlc.State.Live` — the only state that counts as funding.
const STATE_LIVE: u64 = 1;

/// M5: the small block tolerance an independent secondary head may differ from the primary
/// before the cross-read fails closed. Amoy blocks are ~2 s and honest public endpoints usually
/// track within a couple; 12 is generous enough to never flap on honest infra, tight enough that
/// a head-inflating liar is caught. The trusted depth always uses the MORE CONSERVATIVE head.
pub const HEAD_CROSS_TOLERANCE_BLOCKS: u64 = 12;

/// The gateway-backed USDC-leg funding verifier. Construct with the chain reads (live:
/// [`HttpPolygonRpc`]), the deployed HTLC address, and the earliest block worth scanning
/// (the contract's deployment block — earlier logs cannot exist).
pub struct PolygonHtlcVerifier<R: PolygonReads> {
    reads: R,
    /// M5: an OPTIONAL independent second chain-reads source. When set, the primary's `head` is
    /// cross-checked against this endpoint (within [`HEAD_CROSS_TOLERANCE_BLOCKS`]) and the more
    /// conservative head drives the depth — a single lying/MITM'd RPC can no longer inflate depth
    /// to fake "funded + deep". When `None`, today's single-RPC trust assumption holds (ADR-0011).
    secondary: Option<R>,
    htlc: EvmAddress,
    from_block: u64,
}

impl<R: PolygonReads> PolygonHtlcVerifier<R> {
    /// A verifier over `reads` for the `NimmeshHtlc` at `htlc`, scanning logs from `from_block`.
    pub fn new(reads: R, htlc: EvmAddress, from_block: u64) -> Self {
        PolygonHtlcVerifier {
            reads,
            secondary: None,
            htlc,
            from_block,
        }
    }

    /// M5: add an INDEPENDENT secondary chain-reads source (a second Amoy endpoint). A reported
    /// depth is then only trusted when the two heads agree within
    /// [`HEAD_CROSS_TOLERANCE_BLOCKS`]; disagreement reads `Absent` (fail-closed).
    pub fn with_secondary(mut self, secondary: R) -> Self {
        self.secondary = Some(secondary);
        self
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
        let mut head = match self.reads.head() {
            Ok(h) => h,
            Err(_) => return FundingObservation::Absent,
        };
        // M5 cross-read: an independent endpoint's head must agree within tolerance before we
        // trust a depth; the more conservative (lower) head then drives it, so a primary that
        // inflates `head` cannot fake depth. Disagreement beyond tolerance → fail-closed.
        if let Some(sec) = &self.secondary {
            match sec.head() {
                Ok(sh) if head.abs_diff(sh) <= HEAD_CROSS_TOLERANCE_BLOCKS => head = head.min(sh),
                _ => return FundingObservation::Absent,
            }
        }

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
                // Finalized fast-path: an escrow at or below the conservative `finalized`
                // height is deterministically buried — report it maximally deep so the
                // per-chain depth gate clears at once. Anything else (tag unserved, a
                // disagreeing/erroring secondary, an escrow above the finalized height)
                // keeps today's probabilistic depth count.
                let finalized = conservative_finalized(&self.reads, self.secondary.as_ref(), head);
                let confirmations = if finalized.is_some_and(|f| block <= f) {
                    FINALIZED_CONFIRMATIONS
                } else {
                    let depth64 = head.saturating_sub(block).saturating_add(1);
                    u32::try_from(depth64).unwrap_or(u32::MAX)
                };
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

    // C1 note: `chain_backed` deliberately stays the DEFAULT `false` here even though this
    // scan can read the real deployed contract — its `timeout` is the RAW on-chain seconds
    // (no ADR-0010 term mapping), so against term-unit expectations the timeout floor would
    // be vacuous. The live-eligible Amoy verifier is `AmoyHtlcSwapVerifier`, which maps.
}

#[cfg(test)]
#[path = "polygon_verifier_tests.rs"]
mod tests;
