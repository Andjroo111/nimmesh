//! # amoy_swap_verifier — the initiator-side USDC funding gate + Amoy chain seam (A2b)
//!
//! The Polygon half of the live NIM⇄USDC money path, split out of
//! [`crate::live_swap_signer`] (800-line guard): the [`AmoyChain`] seam every live Amoy call
//! rides (so the signer/verifier logic is byte-asserted OFFLINE against fakes; the live impl
//! is the Amoy-guarded [`HttpPolygonRpc`]), the shared [`PolygonFundingStore`], and
//! [`AmoyHtlcSwapVerifier`] — the initiator's funding gate.
//!
//! The verifier is [`crate::polygon_verifier`]'s deployed-contract scan with one live-path
//! twist: the public Amoy RPC caps `eth_getLogs` ranges at ~50 blocks, so a blind lookback
//! cannot work — the scan is **anchored at the `FundingProof`-named funding tx's receipt**
//! (fed in as an untrusted hint via
//! [`FundingVerifier::note_funding_wire`]). A forged hint can only anchor the scan somewhere
//! empty, which reads `Absent` — fail-closed, exactly like no hint at all. The found escrow's
//! `swapId` is recorded so the initiator's claim withdraws the exact verified slot.
//! Timeout-unit mapping per ADR-0010 (the seconds twins of `nim_verifier`'s ms helpers).
//!
//! **Finalized fast-path (ADR-0003 addendum, 2026-07-15):** an escrow whose inclusion block is
//! at or below the chain's `finalized` tag (Polygon Heimdall v2 milestone finality, ~5 s) is
//! reported at [`crate::swap_funding_verify::FINALIZED_CONFIRMATIONS`] — deterministic finality
//! outranks any probabilistic count. A finalized read that fails anywhere (primary or a wired
//! secondary) just keeps today's depth counting: slower, never less safe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::evm::{function_selector, keccak256};
use crate::evm_abi::WORD;
use crate::nimiq::hex::bytes_to_hex;
use crate::polygon_gateway::{EvmLog, EvmReceipt, EvmRpcError, HttpPolygonRpc};
use crate::polygon_verifier::{new_swap_topic0, HEAD_CROSS_TOLERANCE_BLOCKS};
use crate::swap_funding_verify::{
    FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_usdc_leg::EvmAddress;
use crate::swap_wire::SwapLegId;

/// Bound on the FundingProof-named anchor candidates retained (one live swap needs 1; a
/// flooder is capped — an overflow hint is ignored, fail-closed).
const MAX_CANDIDATES: usize = 32;

/// Seconds since the Unix epoch (the default verifier clock).
fn system_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The responder-fund → initiator-verify slack (seconds) for the USDC timelock mapping —
/// the seconds twin of [`crate::nim_verifier::DEFAULT_TIMEOUT_SLACK_S`].
pub const DEFAULT_TIMELOCK_SLACK_S: u64 = 900;
/// How many blocks before the funding receipt's block the Amoy log scan starts (stays well
/// inside the public RPC's ~50-block `eth_getLogs` cap).
pub const DEFAULT_FROM_BLOCK_MARGIN: u64 = 5;
/// The on-chain Amoy timelock (Unix **seconds**) for a leg with term-unit timeout `term`
/// (ADR-0010: term units ≈ seconds relative to the mesh anchor, mapped at fund time).
pub fn usdc_timelock_s(term: u64, term_anchor: u64, now_s: u64) -> u64 {
    now_s.saturating_add(term.saturating_sub(term_anchor))
}

/// The verifier-side inverse: express an on-chain Unix-seconds timelock back in term units
/// (plus `slack_s`), so `require_funded`'s `timeout ≥ min_timeout` enforces the wall-clock
/// floor — the seconds twin of [`crate::nim_verifier::term_equivalent_of_timeout_ms`].
pub fn term_equivalent_of_timelock_s(
    timelock_s: u64,
    now_s: u64,
    term_anchor: u64,
    slack_s: u64,
) -> u64 {
    timelock_s
        .saturating_sub(now_s)
        .saturating_add(slack_s)
        .saturating_add(term_anchor)
}

// --- the Amoy chain seam (offline-testable) -------------------------------------------------------

/// The Amoy reads + writes the live signer/verifier need — a seam so every signing path is
/// byte-asserted OFFLINE against a fake; the live impl is [`HttpPolygonRpc`] (Amoy-guarded).
pub trait AmoyChain: Send + Sync {
    /// `eth_gasPrice` (wei).
    fn gas_price(&self) -> Result<u64, EvmRpcError>;
    /// `eth_getTransactionCount(address, "pending")`.
    fn transaction_count(&self, address: &EvmAddress) -> Result<u64, EvmRpcError>;
    /// `eth_getBalance(address)` (wei).
    fn balance(&self, address: &EvmAddress) -> Result<u128, EvmRpcError>;
    /// `eth_sendRawTransaction` — returns the tx hash hex.
    fn send_raw(&self, raw: &[u8]) -> Result<String, EvmRpcError>;
    /// `eth_getTransactionReceipt` for a 32-byte tx hash; `None` while pending.
    fn receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<EvmReceipt>, EvmRpcError>;
    /// A read-only `eth_call`, decoded result bytes.
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError>;
    /// `eth_estimateGas({from, to, data})` — the gas a call would consume. The default returns a
    /// `BadResponse` so callers that lack a live estimate (offline fakes) transparently fall back to
    /// a fixed cap; [`HttpPolygonRpc`] overrides it with the real `eth_estimateGas`.
    fn estimate_gas(
        &self,
        _from: &EvmAddress,
        _to: &EvmAddress,
        _data: &[u8],
    ) -> Result<u64, EvmRpcError> {
        Err(EvmRpcError::BadResponse {
            method: "eth_estimateGas".to_string(),
        })
    }
    /// `NewSwap` logs on `htlc` whose indexed receiver (topic 3) is `recipient`.
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError>;
    /// `eth_blockNumber`.
    fn head(&self) -> Result<u64, EvmRpcError>;
    /// The highest DETERMINISTICALLY-final block height (the `finalized` tag — Polygon PoS:
    /// Heimdall v2 milestone finality, ~5 s). The default errors, meaning "tag not served" —
    /// the verifier then keeps plain depth counting (strictly slower, never less safe), so
    /// every offline fake predating the tag changes nothing. [`HttpPolygonRpc`] overrides it.
    fn finalized_head(&self) -> Result<u64, EvmRpcError> {
        Err(EvmRpcError::BadResponse {
            method: "eth_getBlockByNumber(finalized)".to_string(),
        })
    }
}

fn addr_hex(a: &EvmAddress) -> String {
    format!("0x{}", bytes_to_hex(a))
}

impl AmoyChain for HttpPolygonRpc {
    fn gas_price(&self) -> Result<u64, EvmRpcError> {
        HttpPolygonRpc::gas_price(self)
    }
    fn transaction_count(&self, address: &EvmAddress) -> Result<u64, EvmRpcError> {
        HttpPolygonRpc::get_transaction_count(self, &addr_hex(address))
    }
    fn balance(&self, address: &EvmAddress) -> Result<u128, EvmRpcError> {
        HttpPolygonRpc::get_balance(self, &addr_hex(address))
    }
    fn send_raw(&self, raw: &[u8]) -> Result<String, EvmRpcError> {
        HttpPolygonRpc::send_raw_transaction(self, &format!("0x{}", bytes_to_hex(raw)))
    }
    fn receipt(&self, tx_hash: &[u8; 32]) -> Result<Option<EvmReceipt>, EvmRpcError> {
        HttpPolygonRpc::get_transaction_receipt(self, &format!("0x{}", bytes_to_hex(tx_hash)))
    }
    fn call(&self, to: &EvmAddress, data: &[u8]) -> Result<Vec<u8>, EvmRpcError> {
        let out = self.eth_call(&addr_hex(to), &format!("0x{}", bytes_to_hex(data)))?;
        crate::nimiq::hex::hex_to_bytes(&out).map_err(|_| EvmRpcError::BadResponse {
            method: "eth_call".to_string(),
        })
    }
    fn estimate_gas(
        &self,
        from: &EvmAddress,
        to: &EvmAddress,
        data: &[u8],
    ) -> Result<u64, EvmRpcError> {
        HttpPolygonRpc::estimate_gas(
            self,
            &addr_hex(from),
            &addr_hex(to),
            &format!("0x{}", bytes_to_hex(data)),
        )
    }
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError> {
        let mut topic3 = [0u8; 32];
        topic3[12..].copy_from_slice(recipient);
        self.get_logs(
            &addr_hex(htlc),
            &format!("0x{}", bytes_to_hex(&new_swap_topic0())),
            Some(&format!("0x{}", bytes_to_hex(&topic3))),
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

/// The finalized height BOTH endpoints vouch for — the `dyn AmoyChain` twin of
/// [`crate::polygon_verifier::conservative_finalized`] (ADR-0003 addendum, 2026-07-15): the
/// primary's `finalized` read, capped by the secondary's when one is wired (min — a lying
/// primary cannot inflate finality past an honest secondary), never beyond the cross-checked
/// `head`. `None` on any failed read → the caller keeps depth counting (never less safe).
fn conservative_finalized(
    primary: &Arc<dyn AmoyChain>,
    secondary: Option<&Arc<dyn AmoyChain>>,
    head: u64,
) -> Option<u64> {
    let fp = primary.finalized_head().ok()?;
    let fin = match secondary {
        None => fp,
        Some(sec) => fp.min(sec.finalized_head().ok()?),
    };
    Some(fin.min(head))
}

/// The low 8 bytes of a 32-byte ABI word.
///
/// LOW (G8 M5): a well-formed word in our domain has its high 24 bytes zero; a word that would
/// overflow `u64` is a malformed/hostile response and reads `None` (fail-closed) rather than
/// SILENTLY TRUNCATING to a plausible low-64-bit number.
fn word_u64(word: &[u8]) -> Option<u64> {
    if word.len() != WORD || word[..WORD - 8].iter().any(|&b| b != 0) {
        return None;
    }
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[WORD - 8..WORD]);
    Some(u64::from_be_bytes(be))
}

/// The `NewSwap` data words `(amount, hashlock, timelock)`; `None` on a foreign shape (or a
/// numeric word that would overflow `u64`).
fn decode_new_swap_data(data: &[u8]) -> Option<(u64, [u8; 32], u64)> {
    if data.len() != 3 * WORD {
        return None;
    }
    let mut hashlock = [0u8; 32];
    hashlock.copy_from_slice(&data[WORD..2 * WORD]);
    Some((
        word_u64(&data[..WORD])?,
        hashlock,
        word_u64(&data[2 * WORD..3 * WORD])?,
    ))
}

/// `NimmeshHtlc.State.Live` — the only state that counts as funding.
pub(crate) const STATE_LIVE: u64 = 1;
/// `NimmeshHtlc.State.Claimed` — what the initiator's landed withdraw leaves behind.
pub(crate) const STATE_CLAIMED: u64 = 2;

/// Read `getSwap(swap_id)`'s state word; `None` on any failure (fail-closed).
pub(crate) fn escrow_state(
    chain: &Arc<dyn AmoyChain>,
    htlc: &EvmAddress,
    swap_id: &[u8; 32],
) -> Option<u64> {
    let mut cd = function_selector("getSwap(bytes32)").to_vec();
    cd.extend_from_slice(swap_id);
    let out = chain.call(htlc, &cd).ok()?;
    if out.len() < 6 * WORD {
        return None;
    }
    word_u64(&out[5 * WORD..6 * WORD])
}

// --- the initiator-side Amoy funding store + verifier ----------------------------------------------

/// One verified Amoy escrow (recorded by [`AmoyHtlcSwapVerifier`] for the claim to reuse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundEscrow {
    /// The on-chain `swapId` (`NewSwap` topic 1) — what `withdraw(S)` needs.
    pub swap_id: [u8; 32],
    /// The escrowed micro-USDC.
    pub amount: u64,
    /// The on-chain timelock (Unix seconds).
    pub timelock_s: u64,
}

/// The initiator's shared Amoy funding state: the `FundingProof`-named candidate funding txs
/// (untrusted hints → the log-scan anchor) and the escrow the verifier actually FOUND on-chain
/// (keyed by hashlock — what the claim withdraws). Shared between [`AmoyHtlcSwapVerifier`] and
/// [`LiveInitiatorSigner`].
#[derive(Default)]
pub struct PolygonFundingStore {
    /// keccak hashes of the claimed funding txs (bounded, deduped).
    candidates: Mutex<Vec<[u8; 32]>>,
    /// The resolved scan anchor: the earliest candidate receipt's block.
    anchor_block: Mutex<Option<u64>>,
    /// hashlock → the verified escrow.
    found: Mutex<HashMap<[u8; 32], FoundEscrow>>,
}

impl PolygonFundingStore {
    /// An empty store.
    pub fn new() -> Self {
        PolygonFundingStore::default()
    }

    /// Note a claimed funding tx wire (its keccak hash becomes an anchor candidate).
    pub fn note_candidate(&self, wire: &[u8]) {
        let hash = keccak256(wire);
        let mut c = self.candidates.lock().unwrap();
        if c.contains(&hash) || c.len() >= MAX_CANDIDATES {
            return;
        }
        c.push(hash);
    }

    /// The verified escrow for a hashlock, if the verifier has found one.
    pub fn found(&self, hashlock: &[u8; 32]) -> Option<FoundEscrow> {
        self.found.lock().unwrap().get(hashlock).copied()
    }

    /// Every verified escrow (A2c: the live e2e reads the — single — escrow to assert its
    /// on-chain resolution and to persist a refund record).
    pub fn found_all(&self) -> Vec<FoundEscrow> {
        self.found.lock().unwrap().values().copied().collect()
    }

    pub(crate) fn record_found(&self, hashlock: [u8; 32], escrow: FoundEscrow) {
        self.found.lock().unwrap().insert(hashlock, escrow);
    }

    /// Resolve (once) and return the log-scan anchor block from the candidates' receipts.
    fn anchor(&self, chain: &Arc<dyn AmoyChain>) -> Option<u64> {
        if let Some(a) = *self.anchor_block.lock().unwrap() {
            return Some(a);
        }
        let candidates = self.candidates.lock().unwrap().clone();
        let mut best: Option<u64> = None;
        for hash in &candidates {
            if let Ok(Some(receipt)) = chain.receipt(hash) {
                if receipt.success {
                    best = Some(
                        best.map_or(receipt.block_number, |b: u64| b.min(receipt.block_number)),
                    );
                }
            }
        }
        if let Some(block) = best {
            *self.anchor_block.lock().unwrap() = Some(block);
        }
        best
    }
}

/// The initiator-side USDC funding gate: [`crate::polygon_verifier`]'s deployed-contract scan,
/// anchored at the `FundingProof`-named funding tx (the public RPC's `eth_getLogs` cap kills
/// blind lookbacks) and recording the found `swapId` for the claim. The expectation's
/// `recipient` field carries the session's chain-agnostic key bytes, so this verifier matches
/// against its **configured own EVM claim address** instead — the address the escrow must pay.
/// Fail-closed everywhere: no anchor yet, any RPC error, or a resolved slot reads `Absent`.
pub struct AmoyHtlcSwapVerifier {
    chain: Arc<dyn AmoyChain>,
    /// M5: an OPTIONAL independent second Amoy chain. When set, the primary's `head` is
    /// cross-checked against it (within [`HEAD_CROSS_TOLERANCE_BLOCKS`], more conservative head
    /// wins) AND the winning escrow is re-read there as still `Live` before it is recorded — a
    /// single lying/MITM'd endpoint can neither inflate depth nor fake a live escrow. When
    /// `None`, the single-RPC trust assumption holds (ADR-0011).
    secondary: Option<Arc<dyn AmoyChain>>,
    htlc: EvmAddress,
    own_claim_address: EvmAddress,
    store: Arc<PolygonFundingStore>,
    slack_s: u64,
    from_block_margin: u64,
    clock_s: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl AmoyHtlcSwapVerifier {
    /// A verifier over `chain` for the deployed HTLC, expecting escrows that pay
    /// `own_claim_address`, sharing `store` with the initiator's signer.
    pub fn new(
        chain: Arc<dyn AmoyChain>,
        htlc: EvmAddress,
        own_claim_address: EvmAddress,
        store: Arc<PolygonFundingStore>,
    ) -> Self {
        AmoyHtlcSwapVerifier {
            chain,
            secondary: None,
            htlc,
            own_claim_address,
            store,
            slack_s: DEFAULT_TIMELOCK_SLACK_S,
            from_block_margin: DEFAULT_FROM_BLOCK_MARGIN,
            clock_s: Box::new(system_now_s),
        }
    }

    /// M5: add an INDEPENDENT secondary Amoy chain (a second endpoint). A reported depth is then
    /// only trusted when the two heads agree within [`HEAD_CROSS_TOLERANCE_BLOCKS`] and the
    /// found escrow re-reads `Live` there; disagreement reads `Absent` (fail-closed).
    pub fn with_secondary(mut self, secondary: Arc<dyn AmoyChain>) -> Self {
        self.secondary = Some(secondary);
        self
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock_s: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock_s = clock_s;
        self
    }
}

impl FundingVerifier for AmoyHtlcSwapVerifier {
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation {
        if expect.leg != SwapLegId::Counterparty {
            return FundingObservation::Absent;
        }
        // No FundingProof-named tx has mined yet → nothing to scan from (fail-closed; the
        // capped public RPC makes an unanchored lookback impossible — module docs).
        let Some(anchor) = self.store.anchor(&self.chain) else {
            return FundingObservation::Absent;
        };
        let from_block = anchor.saturating_sub(self.from_block_margin);
        let logs =
            match self
                .chain
                .new_swap_logs_to(&self.htlc, &self.own_claim_address, from_block)
            {
                Ok(l) => l,
                Err(_) => return FundingObservation::Absent,
            };
        let mut head = match self.chain.head() {
            Ok(h) => h,
            Err(_) => return FundingObservation::Absent,
        };
        // M5 cross-read: an independent endpoint's head must agree within tolerance before we
        // trust a depth; the more conservative (lower) head then drives it. Disagreement beyond
        // tolerance → fail-closed.
        if let Some(sec) = &self.secondary {
            match sec.head() {
                Ok(sh) if head.abs_diff(sh) <= HEAD_CROSS_TOLERANCE_BLOCKS => head = head.min(sh),
                _ => return FundingObservation::Absent,
            }
        }

        let mut other_hashlock_pays_us = false;
        let mut best: Option<(u64, FoundEscrow)> = None; // (block, escrow)
        for log in &logs {
            let Some((amount, hashlock, timelock_s)) = decode_new_swap_data(&log.data) else {
                continue;
            };
            if hashlock != expect.hashlock {
                other_hashlock_pays_us = true;
                continue;
            }
            // The escrow must STILL be live — a claimed/refunded slot is not funding.
            match escrow_state(&self.chain, &self.htlc, &log.topic1) {
                Some(STATE_LIVE) => {}
                _ => continue,
            }
            if best.map_or(true, |(b, _)| log.block_number < b) {
                best = Some((
                    log.block_number,
                    FoundEscrow {
                        swap_id: log.topic1,
                        amount,
                        timelock_s,
                    },
                ));
            }
        }

        match best {
            Some((block, escrow)) => {
                // M5 cross-read: independently confirm the winning escrow STILL reads Live on the
                // secondary before recording it — a primary faking a live escrow is caught.
                if let Some(sec) = &self.secondary {
                    if escrow_state(sec, &self.htlc, &escrow.swap_id) != Some(STATE_LIVE) {
                        return FundingObservation::Absent;
                    }
                }
                self.store.record_found(expect.hashlock, escrow);
                // Finalized fast-path (ADR-0003 addendum, 2026-07-15): an escrow at or below
                // the conservative `finalized` height is deterministically buried — report it
                // maximally deep so the per-chain depth gate clears at once. Tag unserved, a
                // secondary that cannot vouch, or an escrow above the finalized height all
                // keep today's probabilistic depth count (slower, never less safe).
                let finalized = conservative_finalized(&self.chain, self.secondary.as_ref(), head);
                let confirmations = if finalized.is_some_and(|f| block <= f) {
                    crate::swap_funding_verify::FINALIZED_CONFIRMATIONS
                } else {
                    u32::try_from(head.saturating_sub(block).saturating_add(1)).unwrap_or(u32::MAX)
                };
                FundingObservation::Found {
                    amount: escrow.amount,
                    timeout: term_equivalent_of_timelock_s(
                        escrow.timelock_s,
                        (self.clock_s)(),
                        // M4: the anchor rides the expectation (= the swap's SwapContext),
                        // never a constructor-frozen zero.
                        expect.term_anchor,
                        self.slack_s,
                    ),
                    confirmations,
                }
            }
            None if other_hashlock_pays_us => {
                FundingObservation::Mismatch(MismatchReason::Hashlock)
            }
            None => FundingObservation::Absent,
        }
    }

    fn note_funding_wire(&self, leg: SwapLegId, tx_wire: &[u8]) {
        if leg == SwapLegId::Counterparty {
            self.store.note_candidate(tx_wire);
        }
    }

    fn chain_backed(&self) -> bool {
        true // C1: reads the DEPLOYED Amoy HTLC — live-signer eligible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_u64_refuses_over_64_bit_words_instead_of_truncating() {
        // A domain word (high 24 bytes zero) decodes; a word with ANY byte set above the low 8
        // reads None (LOW / G8 M5 — never a silent truncation of a > 2^64 value).
        let mut w = [0u8; WORD];
        w[WORD - 8..].copy_from_slice(&999_u64.to_be_bytes());
        assert_eq!(word_u64(&w), Some(999));
        w[WORD - 9] = 0x01; // the byte just above the low 8 → would overflow u64
        assert_eq!(word_u64(&w), None);
        assert_eq!(word_u64(&[0u8; WORD - 1]), None); // malformed short word
    }

    #[test]
    fn decode_new_swap_data_rejects_an_over_u64_timelock_word() {
        // Shape-valid 3-word payload, but the timelock word overflows u64 → the whole decode
        // is None (the log is skipped upstream, never advanced on a truncated value).
        let mut data = vec![0u8; 3 * WORD];
        data[WORD - 8..WORD].copy_from_slice(&1_000_000_u64.to_be_bytes()); // amount, valid
        data[2 * WORD] = 0x01; // high byte of the timelock word set
        assert_eq!(decode_new_swap_data(&data), None);
    }
}
