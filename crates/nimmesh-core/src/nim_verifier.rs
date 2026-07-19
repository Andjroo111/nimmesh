//! # nim_verifier — the NIM-RPC-backed [`FundingVerifier`] (#72 tail, the NIM leg; A2b)
//!
//! The real-chain funding gate for the **NIM leg**: before a responder funds its own
//! counterparty leg, [`NimHtlcVerifier`] confirms the initiator's NIM HTLC is really on the
//! Albatross testnet with the agreed terms — [`crate::swap_funding_verify::LedgerVerifier`]
//! semantics against actual chain state, the NIM sibling of
//! [`crate::polygon_verifier::PolygonHtlcVerifier`].
//!
//! ## How it locates the HTLC (the hint + hash-binding argument)
//!
//! A NIM HTLC lives at a **content-derived** contract address
//! (`Blake2b-256(serializeContent with zeroed recipient)[..20]`,
//! [`HtlcCreation::contract_address`]), and its canonical tx hash is
//! `Blake2b-256(serializeContent)` — both collision-resistant digests of the FULL creation
//! transaction. So the peer's `FundingProof` wire (fed in via
//! [`FundingVerifier::note_funding_wire`]) is decoded as an **untrusted locating hint**, and
//! everything that matters is then re-established from chain truth:
//!
//! 1. decode the wire ([`decode_creation_wire`]) → the claimed hashlock / recipient / amount /
//!    timeout / network are checked against the [`HtlcExpectation`] **first** (a lie here is a
//!    `Mismatch`/`Absent`, and nothing on-chain is even consulted);
//! 2. `getTransactionByHash(Blake2b(content))` — inclusion + depth. Because the hash IS the
//!    content digest, the included tx can only be the exact decoded creation (a different tx
//!    cannot share the hash), so the decoded parameters are **bound** to the on-chain tx;
//! 3. `getAccountByAddress(contract)` — the derived contract account must exist as an HTLC
//!    holding at least the agreed amount. A claimed/refunded (emptied) contract reads
//!    [`FundingObservation::Absent`] — a resolved slot is not funding, mirroring the ledger
//!    reference and the Polygon verifier.
//!
//! **Fail-closed:** no hint yet, an RPC failure, or anything unexpected reads `Absent` — the
//! gate then refuses (`NotFundedYet`) and retries on the next `FundingProof` retransmit. A
//! transport blip can delay a swap; it can never authorize one.
//!
//! ## Timeout-unit mapping (ADR-0010)
//!
//! Session terms (`T_A`/`T_B`) are mesh-anchored second-granularity units; the on-chain NIM
//! HTLC timeout is a Unix-**millisecond** timestamp. `observe` reports the on-chain timeout
//! converted BACK into term units — `(timeout_ms − now_ms)/1000 + slack + anchor` — so
//! [`require_funded`](crate::swap_funding_verify::require_funded)'s single
//! `timeout ≥ min_timeout` gate becomes exactly the intended wall-clock floor
//! `timeout_ms ≥ now_ms + (T − anchor − slack)·1000`. The slack absorbs the wall-clock drift
//! between the funder's mapping and this verifier's re-check (default 15 min, well under the
//! ladder's margins).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nimiq::address::Address;
use crate::nimiq::hex::bytes_to_hex;
use crate::nimiq::htlc::{decode_creation_wire, HashAlgorithm, HtlcCreation};
use crate::rpc::GatewayRpc;
use crate::swap_funding_verify::{
    ClaimObservation, FundingObservation, FundingVerifier, HtlcExpectation, MismatchReason,
};
use crate::swap_wire::SwapLegId;

/// Default verify-side slack (seconds) for the term↔wall-clock timeout mapping: the floor the
/// verifier enforces is `T − slack` term units ahead of ITS clock, absorbing the delay between
/// the funder computing the on-chain timeout and this node re-checking it. 15 minutes — far
/// above any honest fund→verify latency, far below the ladder's hour-scale margins.
pub const DEFAULT_TIMEOUT_SLACK_S: u64 = 900;

/// Bound on distinct funding hints the store retains (one per hashlock; a session runs at most
/// [`crate::swap_session::DEFAULT_MAX_CONCURRENT_SWAPS`] swaps, and a flooder past the bound is
/// simply ignored — fail-closed).
const MAX_STORE_ENTRIES: usize = 32;

/// The funder's on-chain timeout for a leg with term-unit timeout `term`: map the term delta
/// (relative to the mesh anchor the terms were built against, ~1 unit ≈ 1 s) onto this node's
/// wall clock as a Unix-**millisecond** timestamp (what the NIM HTLC field carries; ADR-0010).
pub fn nim_htlc_timeout_ms(term: u64, term_anchor: u64, now_ms: u64) -> u64 {
    now_ms.saturating_add(term.saturating_sub(term_anchor).saturating_mul(1000))
}

/// The verifier-side inverse: express an on-chain Unix-ms timeout back in term units (plus the
/// verify-side `slack_s`), so `require_funded`'s `timeout ≥ min_timeout` check enforces the
/// wall-clock floor `timeout_ms ≥ now_ms + (min_timeout − anchor − slack)·1000`.
pub fn term_equivalent_of_timeout_ms(
    timeout_ms: u64,
    now_ms: u64,
    term_anchor: u64,
    slack_s: u64,
) -> u64 {
    (timeout_ms.saturating_sub(now_ms) / 1000)
        .saturating_add(slack_s)
        .saturating_add(term_anchor)
}

/// One decoded, shape-valid NIM funding hint: the creation tx as claimed by a `FundingProof`,
/// with its content-derived contract address and canonical tx hash precomputed. Everything in
/// here is UNTRUSTED until [`NimHtlcVerifier::observe`] re-establishes it from the chain.
#[derive(Debug, Clone)]
pub struct NimFundingRecord {
    /// The decoded creation transaction (hashlock, recipient, amount, ms-timeout, network…).
    pub creation: HtlcCreation,
    /// The content-derived HTLC contract address the funds live at.
    pub contract: Address,
    /// The canonical tx hash (`Blake2b-256(serializeContent)`), lowercase hex.
    pub tx_hash_hex: String,
}

/// The shared drawer of decoded NIM funding hints, keyed by hashlock — filled by the verifier's
/// [`FundingVerifier::note_funding_wire`], read back by the verifier's `observe` and by the
/// responder's live signer (which needs the same contract address + amount to build its claim).
/// First hint wins per hashlock (mirroring the coordinator's first-`Accept`-wins) and the table
/// is bounded, so a flooder can neither swap the hint under us nor grow memory.
#[derive(Default)]
pub struct NimFundingStore {
    entries: Mutex<HashMap<[u8; 32], NimFundingRecord>>,
}

impl NimFundingStore {
    /// An empty store.
    pub fn new() -> Self {
        NimFundingStore::default()
    }

    /// Decode + retain a claimed NIM funding wire (first hint per hashlock wins; bounded).
    /// Anything that is not a well-formed SHA-256/hash-count-1 HTLC creation is dropped.
    pub fn note_wire(&self, wire: &[u8]) {
        let Some(creation) = decode_creation_wire(wire) else {
            return;
        };
        if creation.data.hash_algorithm != HashAlgorithm::Sha256 || creation.data.hash_count != 1 {
            return; // not the cross-chain HTLC shape this swap uses
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&creation.data.hash_root) {
            return; // first hint wins
        }
        if entries.len() >= MAX_STORE_ENTRIES {
            return; // bounded — an overflow hint is ignored (fail-closed)
        }
        entries.insert(
            creation.data.hash_root,
            NimFundingRecord {
                creation,
                contract: creation.contract_address(),
                tx_hash_hex: bytes_to_hex(&creation.tx_hash()),
            },
        );
    }

    /// The retained hint for a hashlock, if any.
    pub fn get(&self, hashlock: &[u8; 32]) -> Option<NimFundingRecord> {
        self.entries.lock().unwrap().get(hashlock).cloned()
    }

    /// Every retained hint (A2c: the live e2e reads the — single — funding it observed to
    /// verify the claim on-chain and to persist a refund record; it never learns `S`).
    pub fn records(&self) -> Vec<NimFundingRecord> {
        self.entries.lock().unwrap().values().cloned().collect()
    }

    /// Whether any retained hint pays `recipient` under a hashlock OTHER than `hashlock` —
    /// the ledger-reference `Mismatch(Hashlock)` signal (an HTLC paying us under a different
    /// lock is never ours to advance on).
    fn pays_recipient_under_other_hashlock(&self, recipient: &[u8], hashlock: &[u8; 32]) -> bool {
        self.entries.lock().unwrap().iter().any(|(h, rec)| {
            h != hashlock && rec.creation.data.htlc_recipient.as_bytes() == recipient
        })
    }
}

/// The inclusion block of the content-bound tx as `rpc` reports it, or `Err(())` on any
/// fail-closed condition: a transport error, an unknown tx, or a returned tx whose reported
/// hash is NOT the expected `Blake2b(content)` digest (M5 content-hash bind). `Ok(None)` means
/// the tx is known but still in the mempool. Shared by the primary read and the cross-read so
/// both endpoints are held to the identical content binding.
fn content_bound_block(rpc: &Arc<dyn GatewayRpc>, expected_hash: &str) -> Result<Option<u32>, ()> {
    match rpc.get_transaction(expected_hash) {
        Err(_) => Err(()),
        Ok(None) => Err(()),
        Ok(Some(tx)) if !tx.hash.eq_ignore_ascii_case(expected_hash) => Err(()),
        Ok(Some(tx)) => Ok(tx.block_number),
    }
}

/// Milliseconds since the Unix epoch (the default verifier clock).
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The NIM-RPC-backed [`FundingVerifier`]. Construct with any [`GatewayRpc`] (the offline tests
/// inject [`crate::rpc::MockRpc`]; the live rig injects the testnet-guarded
/// [`HttpGatewayRpc`](crate::rpc::HttpGatewayRpc)) and a [`NimFundingStore`] — share the store
/// with the responder's live signer so the claim reuses the exact verified contract.
pub struct NimHtlcVerifier {
    rpc: Arc<dyn GatewayRpc>,
    /// M5: an OPTIONAL independent second RPC. When set, a reported depth is only trusted if
    /// this endpoint AGREES on the inclusion block, and its head folds into a conservative
    /// depth — so a single compromised/MITM'd endpoint can no longer fake "NIM funded + deep".
    /// When `None`, today's single-RPC trust assumption holds (documented in ADR-0011).
    secondary: Option<Arc<dyn GatewayRpc>>,
    store: Arc<NimFundingStore>,
    /// Verify-side slack for the timeout mapping, in seconds.
    timeout_slack_s: u64,
    /// The Albatross network the funding tx must be for (pinned testnet by the constructor).
    network_id: u8,
    /// The wall clock (injectable so the mapping tests are deterministic).
    clock_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl NimHtlcVerifier {
    /// A verifier over `rpc`, retaining hints in `store`. TESTNET-pinned: only a funding tx
    /// stamped with the Albatross testnet network id can ever verify.
    pub fn new(rpc: Arc<dyn GatewayRpc>, store: Arc<NimFundingStore>) -> Self {
        NimHtlcVerifier {
            rpc,
            secondary: None,
            store,
            timeout_slack_s: DEFAULT_TIMEOUT_SLACK_S,
            network_id: crate::NetworkId::Testnet.wire_id(),
            clock_ms: Box::new(system_now_ms),
        }
    }

    /// **OWNER-GATED (real money): the MAINNET NIM verifier.** Identical to [`Self::new`] but pins
    /// the funding-tx network to `NetworkId::Mainnet` — a funding tx stamped testnet can then never
    /// verify. Exists SOLELY for the off-by-default mainnet swap path (`crate::mainnet_swap`); the
    /// autonomous loop and every default constructor use [`Self::new`] (testnet-pinned). ADR-0011 §4.3
    /// still applies: on mainnet the secondary cross-read should be a node the operator controls (the
    /// NIM leg otherwise runs single-RPC — only `rpc.nimiqwatch.com` is public; see the guard-lift PR).
    pub fn new_mainnet(rpc: Arc<dyn GatewayRpc>, store: Arc<NimFundingStore>) -> Self {
        NimHtlcVerifier {
            rpc,
            secondary: None,
            store,
            timeout_slack_s: DEFAULT_TIMEOUT_SLACK_S,
            network_id: crate::NetworkId::Mainnet.wire_id(),
            clock_ms: Box::new(system_now_ms),
        }
    }

    /// M5: add an INDEPENDENT secondary RPC (a second public testnet endpoint). The reported
    /// inclusion depth is then only trusted when this endpoint agrees on the inclusion block;
    /// disagreement (or an unseen/mismatched tx) reads `Absent` (fail-closed). Pass a genuinely
    /// different endpoint so a lie by either side is caught.
    pub fn with_secondary(mut self, secondary: Arc<dyn GatewayRpc>) -> Self {
        self.secondary = Some(secondary);
        self
    }

    /// Override the verify-side timeout slack (seconds).
    pub fn with_timeout_slack(mut self, slack_s: u64) -> Self {
        self.timeout_slack_s = slack_s;
        self
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock_ms: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock_ms = clock_ms;
        self
    }

    fn observe_nim(&self, expect: &HtlcExpectation) -> FundingObservation {
        // The hint gate: no decoded funding for this hashlock yet → nothing to verify.
        let Some(rec) = self.store.get(&expect.hashlock) else {
            if self
                .store
                .pays_recipient_under_other_hashlock(&expect.recipient, &expect.hashlock)
            {
                return FundingObservation::Mismatch(MismatchReason::Hashlock);
            }
            return FundingObservation::Absent;
        };
        // Decode-time term checks (bound to the chain below by the content hash):
        if rec.creation.data.htlc_recipient.as_bytes().as_slice() != expect.recipient.as_slice() {
            return FundingObservation::Mismatch(MismatchReason::Recipient);
        }
        if rec.creation.network_id != self.network_id {
            return FundingObservation::Absent; // wrong chain — never ours to advance on
        }

        // Chain truth #1 — inclusion + depth of the exact claimed creation tx.
        //
        // M5 (content-hash bind): the canonical NIM tx hash IS `Blake2b-256(serializeContent)`,
        // so we RECOMPUTE it from the decoded creation and require the RPC-returned tx to report
        // EXACTLY that digest before its `block_number` is trusted (a returned tx whose identity
        // is not our content digest reads `Absent`). M5 (cross-read): when a secondary endpoint
        // is configured it must AGREE on the inclusion block, and its head folds into a
        // conservative depth — so a single lying endpoint can neither fake inclusion nor inflate
        // `head` to fake depth. Both re-establish chain truth from an independent source.
        let expected_hash = bytes_to_hex(&rec.creation.tx_hash());
        let block = match content_bound_block(&self.rpc, &expected_hash) {
            Ok(b) => b,
            Err(()) => return FundingObservation::Absent, // transport / unseen / hash mismatch
        };
        let mut head = match self.rpc.block_number() {
            Err(_) => return FundingObservation::Absent,
            Ok(h) => h,
        };
        if let Some(sec) = &self.secondary {
            match content_bound_block(sec, &expected_hash) {
                Ok(sec_block) if sec_block == block => {} // independent agreement on inclusion
                _ => return FundingObservation::Absent,   // disagreement → fail-closed
            }
            match sec.block_number() {
                Ok(sh) => head = head.min(sh), // the more conservative (shallower) depth wins
                Err(_) => return FundingObservation::Absent,
            }
        }
        let confirmations = match block {
            None => 0, // known, still in the mempool
            Some(b) => head.saturating_sub(b).saturating_add(1),
        };

        // Chain truth #2 — once included, the content-derived contract account must exist as an
        // HTLC still holding at least the created amount (an emptied slot is not funding).
        if confirmations > 0 {
            match self.rpc.get_account(&rec.contract.to_user_friendly()) {
                Err(_) => return FundingObservation::Absent,
                Ok(None) => return FundingObservation::Absent, // resolved + pruned
                Ok(Some(acct)) => {
                    if !acct.account_type.eq_ignore_ascii_case("htlc")
                        || acct.balance < rec.creation.value
                    {
                        return FundingObservation::Absent; // claimed/refunded/partial — not funding
                    }
                }
            }
        }

        FundingObservation::Found {
            amount: rec.creation.value,
            timeout: term_equivalent_of_timeout_ms(
                rec.creation.data.timeout,
                (self.clock_ms)(),
                // M4: the anchor rides the expectation (= the swap's SwapContext), never a
                // constructor-frozen zero.
                expect.term_anchor,
                self.timeout_slack_s,
            ),
            confirmations,
        }
    }
}

impl FundingVerifier for NimHtlcVerifier {
    fn observe(&self, expect: &HtlcExpectation) -> FundingObservation {
        // This verifier speaks only the NIM leg; the counterparty leg has its own gateway.
        if expect.leg != SwapLegId::Nim {
            return FundingObservation::Absent;
        }
        self.observe_nim(expect)
    }

    fn note_funding_wire(&self, leg: SwapLegId, tx_wire: &[u8]) {
        if leg == SwapLegId::Nim {
            self.store.note_wire(tx_wire);
        }
    }

    fn chain_backed(&self) -> bool {
        true // C1: reads the real Albatross testnet — live-signer eligible
    }

    /// Run-4 fix: the responder's claim watch — `getTransactionByHash` on our OWN broadcast NIM
    /// redeem. A POSITIVE inclusion signal, never inferred from the HTLC account vanishing (a
    /// gone contract is ambiguous with a refund, and an RPC failure must never read as either).
    /// Three-valued, fail-closed:
    /// - included at block `b` → `Included { head − b + 1 }` (0 while in the mempool, or when the
    ///   head read fails — included-but-depth-unknown stays shallow rather than lying deep);
    /// - the RPC affirmatively does not know the tx → `NotFound` (the caller may re-claim);
    /// - transport failure, a mismatched reported hash, or (M5) secondary disagreement →
    ///   `Unavailable` (change nothing; retry next tick).
    fn observe_claim(&self, leg: SwapLegId, tx_id: &[u8; 32]) -> ClaimObservation {
        if leg != SwapLegId::Nim {
            return ClaimObservation::Unavailable; // this verifier speaks only the NIM leg
        }
        let hash = bytes_to_hex(tx_id);
        let block = match self.rpc.get_transaction(&hash) {
            Err(_) => return ClaimObservation::Unavailable,
            Ok(None) => None,
            Ok(Some(tx)) if !tx.hash.eq_ignore_ascii_case(&hash) => {
                return ClaimObservation::Unavailable; // not our digest — trust nothing off it
            }
            Ok(Some(tx)) => Some(tx.block_number),
        };
        // M5: when a secondary endpoint is configured it must AGREE before we trust either a
        // NotFound (which re-arms a re-broadcast) or an inclusion (which authorizes a settle).
        if let Some(sec) = &self.secondary {
            match sec.get_transaction(&hash) {
                Err(_) => return ClaimObservation::Unavailable,
                Ok(None) if block.is_none() => {}
                Ok(Some(tx))
                    if tx.hash.eq_ignore_ascii_case(&hash) && Some(tx.block_number) == block => {}
                _ => return ClaimObservation::Unavailable, // disagreement → fail-closed
            }
        }
        match block {
            None => ClaimObservation::NotFound,
            Some(None) => ClaimObservation::Included { confirmations: 0 }, // known, mempool
            Some(Some(b)) => {
                let mut head = match self.rpc.block_number() {
                    Err(_) => return ClaimObservation::Included { confirmations: 0 },
                    Ok(h) => h,
                };
                if let Some(sec) = &self.secondary {
                    match sec.block_number() {
                        Ok(sh) => head = head.min(sh), // conservative (shallower) depth wins
                        Err(_) => return ClaimObservation::Included { confirmations: 0 },
                    }
                }
                ClaimObservation::Included {
                    confirmations: head.saturating_sub(b).saturating_add(1),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "nim_verifier_tests.rs"]
mod tests;
