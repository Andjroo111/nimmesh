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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::evm::{function_selector, keccak256};
use crate::evm_abi::WORD;
use crate::nimiq::hex::bytes_to_hex;
use crate::polygon_gateway::{EvmLog, EvmReceipt, EvmRpcError, HttpPolygonRpc};
use crate::polygon_verifier::new_swap_topic0;
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
    /// `NewSwap` logs on `htlc` whose indexed receiver (topic 3) is `recipient`.
    fn new_swap_logs_to(
        &self,
        htlc: &EvmAddress,
        recipient: &EvmAddress,
        from_block: u64,
    ) -> Result<Vec<EvmLog>, EvmRpcError>;
    /// `eth_blockNumber`.
    fn head(&self) -> Result<u64, EvmRpcError>;
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
}

/// The low 8 bytes of a 32-byte ABI word.
fn word_u64(word: &[u8]) -> u64 {
    let mut be = [0u8; 8];
    be.copy_from_slice(&word[WORD - 8..WORD]);
    u64::from_be_bytes(be)
}

/// The `NewSwap` data words `(amount, hashlock, timelock)`; `None` on a foreign shape.
fn decode_new_swap_data(data: &[u8]) -> Option<(u64, [u8; 32], u64)> {
    if data.len() != 3 * WORD {
        return None;
    }
    let mut hashlock = [0u8; 32];
    hashlock.copy_from_slice(&data[WORD..2 * WORD]);
    Some((
        word_u64(&data[..WORD]),
        hashlock,
        word_u64(&data[2 * WORD..3 * WORD]),
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
    Some(word_u64(&out[5 * WORD..6 * WORD]))
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
    htlc: EvmAddress,
    own_claim_address: EvmAddress,
    store: Arc<PolygonFundingStore>,
    term_anchor: u64,
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
            htlc,
            own_claim_address,
            store,
            term_anchor: 0,
            slack_s: DEFAULT_TIMELOCK_SLACK_S,
            from_block_margin: DEFAULT_FROM_BLOCK_MARGIN,
            clock_s: Box::new(system_now_s),
        }
    }

    /// Set the mesh-head anchor the session terms are relative to (default 0).
    pub fn with_term_anchor(mut self, anchor: u64) -> Self {
        self.term_anchor = anchor;
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
        let head = match self.chain.head() {
            Ok(h) => h,
            Err(_) => return FundingObservation::Absent,
        };

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
                self.store.record_found(expect.hashlock, escrow);
                FundingObservation::Found {
                    amount: escrow.amount,
                    timeout: term_equivalent_of_timelock_s(
                        escrow.timelock_s,
                        (self.clock_s)(),
                        self.term_anchor,
                        self.slack_s,
                    ),
                    confirmations: u32::try_from(head.saturating_sub(block).saturating_add(1))
                        .unwrap_or(u32::MAX),
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
}
