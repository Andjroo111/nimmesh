//! # live_swap_signer — the LIVE NIM⇄USDC money-path signer (A2b; testnet/Amoy only)
//!
//! The first real [`SwapSigner`]: the drop-in that turns the mesh swap protocol's
//! phase-driven actions into **actual on-chain transactions** on the Albatross TESTNET and
//! Polygon **Amoy** — the two chains the whole stack has already proven byte-exactly
//! (`nimiq::htlc` live on testnet; `evm_*`/`polygon_gateway` live on Amoy, `docs/swap/AMOY.md`).
//! Two roles, two signers:
//!
//! - [`LiveInitiatorSigner`] — the NIM-giver. At `Accepted` it builds + broadcasts the real
//!   NIM HTLC creation (recipient = the responder's protocol-carried NIM claim address); at
//!   `BothFunded` it lands the real Amoy `withdraw(S)` (revealing `S` on-chain) and returns
//!   the `S ‖ raw-tx` wire the responder reads the secret off.
//! - [`LiveResponderSigner`] — the USDC-giver. At `InitiatorFunded` (i.e. only AFTER its
//!   [`NimHtlcVerifier`](crate::nim_verifier::NimHtlcVerifier) confirmed the NIM HTLC
//!   on-chain) it lands `approve` + `newSwap` (receiver = the initiator's protocol-carried
//!   EVM claim address); at `Revealed` it claims the NIM HTLC with `S`.
//!
//! Plus [`AmoyHtlcSwapVerifier`] — the initiator-side funding gate: the deployed-contract
//! log scan of [`crate::polygon_verifier`], but **anchored at the `FundingProof`'s named tx**
//! (the public Amoy RPC caps `eth_getLogs` ranges at ~50 blocks, so a blind lookback cannot
//! work — the receipt of the claimed funding tx IS the anchor), and it records the found
//! escrow's `swapId` so the initiator's claim withdraws the exact verified slot.
//!
//! Blocking chain IO runs inside `build_*` on the node's worker thread — accepted on a rig
//! node (the phone keeps `MockSigner` until its own live wiring lands). Every chain client is
//! constructor-guarded (`guard_testnet` / `guard_amoy`); [`fund_nim`](LiveInitiatorSigner)
//! additionally refuses any context not stamped for the Albatross testnet. Term↔wall-clock
//! timeout mapping per ADR-0010 (`nim_verifier` has the ms helpers; the seconds twins live
//! in [`crate::amoy_swap_verifier`], re-exported here). No key material is ever printed or
//! serialized.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::amoy_swap_verifier::{escrow_state, STATE_CLAIMED, STATE_LIVE};
use crate::evm::keccak256;
use crate::evm_abi::{erc20_approve, htlc_new_swap, htlc_withdraw};
use crate::evm_rlp::LegacyTx;
use crate::evm_signer::LocalEvmKey;
use crate::nim_verifier::{nim_htlc_timeout_ms, NimFundingStore};
use crate::nimiq::address::Address;
use crate::nimiq::hex::bytes_to_hex;
use crate::nimiq::htlc::{
    regular_transfer_proof, HashAlgorithm, HtlcCreation, HtlcCreationData, HtlcRedeem,
};
use crate::nimiq::signer::EnclaveKey;
use crate::nimiq::tx::signature_proof_single_sig;
use crate::polygon_gateway::EvmReceipt;
use crate::rpc::GatewayRpc;
use crate::swap_coordinator::SwapContext;
use crate::swap_signer::SwapSigner;
use crate::swap_usdc_leg::{usdc_swap_id, EvmAddress};
use crate::swap_wire::{SwapLegId, NIM_ADDRESS_LEN, SWAP_ID_LEN};

// The Polygon half lives in its own module (800-line guard); re-export the public surface so
// callers keep ONE import path for the live NIM⇄USDC stack.
pub use crate::amoy_swap_verifier::{
    term_equivalent_of_timelock_s, usdc_timelock_s, AmoyChain, AmoyHtlcSwapVerifier, FoundEscrow,
    PolygonFundingStore, DEFAULT_FROM_BLOCK_MARGIN, DEFAULT_TIMELOCK_SLACK_S,
};

/// Bound on the peer book (one live swap needs 1 entry; a flooder is capped).
const MAX_BOOK_ENTRIES: usize = 32;

/// A landed responder funding: `(newSwap raw wire, tx id, on-chain swapId)`.
type LandedFunding = (Vec<u8>, [u8; 32], [u8; 32]);

/// Seconds since the Unix epoch (the default live clock).
fn system_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Milliseconds since the Unix epoch.
fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --- the peer book (the note_peer seam's live sink) ----------------------------------------------

/// The counterparty addressing one swap carried over the protocol (`Propose`/`Accept`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddressing {
    /// The peer's NIM address (initiator: its refund address; responder: its claim address).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// The peer's counterparty-chain address bytes (for this pair: a 20-byte EVM address).
    pub chain_address: Vec<u8>,
}

/// The live sink for [`SwapSigner::note_peer`]: swap_id → the peer's protocol-carried payout
/// addressing. First report wins (mirroring the coordinator's first-`Accept`-wins) and the
/// table is bounded.
#[derive(Default)]
pub struct PeerBook {
    entries: Mutex<HashMap<[u8; SWAP_ID_LEN], PeerAddressing>>,
}

impl PeerBook {
    /// An empty book.
    pub fn new() -> Self {
        PeerBook::default()
    }

    /// Record a peer's addressing for a swap (first report wins; bounded).
    pub fn note(&self, swap_id: [u8; SWAP_ID_LEN], peer: PeerAddressing) {
        let mut entries = self.entries.lock().unwrap();
        if entries.contains_key(&swap_id) || entries.len() >= MAX_BOOK_ENTRIES {
            return;
        }
        entries.insert(swap_id, peer);
    }

    /// The recorded addressing for a swap, if any.
    pub fn get(&self, swap_id: &[u8; SWAP_ID_LEN]) -> Option<PeerAddressing> {
        self.entries.lock().unwrap().get(swap_id).cloned()
    }
}

// --- gas + polling knobs --------------------------------------------------------------------------

/// Gas policy for the Amoy legs — the clamps + limits the live G6/G7 runs proved
/// (`docs/swap/AMOY.md`): clamp the node's suggestion to `[30, 50]` gwei, per-call limits
/// sized from real receipts (the proxied USDC makes approve/newSwap heavier than bare ERC-20s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmGasConfig {
    /// Floor for the clamped gas price (wei).
    pub min_gas_price: u64,
    /// Cap for the clamped gas price (wei) — Amoy fee spikes are real.
    pub max_gas_price: u64,
    /// Gas limit for `approve`.
    pub gas_approve: u64,
    /// Gas limit for `newSwap`.
    pub gas_new_swap: u64,
    /// Gas limit for `withdraw`.
    pub gas_withdraw: u64,
    /// Reserve kept for a later `refund` in the budget preflight (never strand an escrow
    /// because the refund can't be paid for).
    pub gas_refund_reserve: u64,
}

impl Default for EvmGasConfig {
    fn default() -> Self {
        EvmGasConfig {
            min_gas_price: 30_000_000_000,
            max_gas_price: 50_000_000_000,
            gas_approve: 90_000,
            gas_new_swap: 280_000,
            gas_withdraw: 170_000,
            gas_refund_reserve: 140_000,
        }
    }
}

/// Bounded chain-polling knobs for the blocking waits inside `build_*` (`interval_ms == 0`
/// skips the sleep — the offline tests run instantly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivePollConfig {
    /// How many poll attempts before giving up.
    pub attempts: u32,
    /// Sleep between attempts, in milliseconds.
    pub interval_ms: u64,
}

impl Default for LivePollConfig {
    fn default() -> Self {
        LivePollConfig {
            attempts: 60,
            interval_ms: 3_000,
        }
    }
}

impl LivePollConfig {
    fn pause(&self) {
        if self.interval_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.interval_ms));
        }
    }
}

// --- shared EVM helpers ---------------------------------------------------------------------------

/// Sign + broadcast one Amoy legacy tx; returns `(raw, keccak-tx-hash)`.
#[allow(clippy::too_many_arguments)]
fn send_evm_tx(
    chain: &Arc<dyn AmoyChain>,
    key: &LocalEvmKey,
    nonce: u64,
    gas_price: u64,
    gas_limit: u64,
    to: &EvmAddress,
    data: &[u8],
    label: &str,
) -> Option<(Vec<u8>, [u8; 32])> {
    let tx = LegacyTx::polygon_amoy(nonce, gas_price, gas_limit, *to, 0, data);
    let raw = tx.sign_with(key);
    let hash = keccak256(&raw);
    match chain.send_raw(&raw) {
        Ok(_) => Some((raw, hash)),
        Err(e) => {
            // The node may already hold it from a prior attempt — a receipt or a pending
            // known-tx means the broadcast is effectively done.
            if matches!(chain.receipt(&hash), Ok(Some(_))) {
                return Some((raw, hash));
            }
            eprintln!("[live-swap] {label}: broadcast failed: {e}");
            None
        }
    }
}

/// Poll a receipt until mined or the budget runs out.
fn wait_receipt(
    chain: &Arc<dyn AmoyChain>,
    tx_hash: &[u8; 32],
    poll: &LivePollConfig,
    label: &str,
) -> Option<EvmReceipt> {
    for _ in 0..poll.attempts.max(1) {
        match chain.receipt(tx_hash) {
            Ok(Some(receipt)) => return Some(receipt),
            Ok(None) => {}
            Err(e) => eprintln!("[live-swap] {label}: receipt poll error (transient): {e}"),
        }
        poll.pause();
    }
    eprintln!("[live-swap] {label}: tx 0x{} not mined in time", {
        bytes_to_hex(tx_hash)
    });
    None
}

// --- the initiator signer ---------------------------------------------------------------------

/// Everything the NIM-giver's live signer needs at construction.
pub struct LiveInitiatorConfig {
    /// The funded NIM account key (funds + refunds the NIM HTLC). The seed never leaves it.
    pub nim_key: Arc<dyn EnclaveKey>,
    /// The testnet-guarded Albatross RPC.
    pub nim_rpc: Arc<dyn GatewayRpc>,
    /// The Amoy account that pays the claim's gas (the caller-open `withdraw` means the payer
    /// and the payout address may differ — ADR-0007).
    pub evm_gas_key: LocalEvmKey,
    /// The Amoy chain client (guarded).
    pub amoy: Arc<dyn AmoyChain>,
    /// The deployed `NimmeshHtlc` this swap escrows in.
    pub htlc: EvmAddress,
    /// Shared with this node's [`AmoyHtlcSwapVerifier`] (the verified `swapId` drives the claim).
    pub store: Arc<PolygonFundingStore>,
    /// Shared peer-addressing sink (filled via [`SwapSigner::note_peer`]).
    pub peer_book: Arc<PeerBook>,
    /// Gas policy.
    pub gas: EvmGasConfig,
    /// Blocking-wait policy.
    pub poll: LivePollConfig,
    /// The mesh-head anchor the session terms are relative to (ADR-0010; 0 on a beacon-silent rig).
    pub term_anchor: u64,
}

/// The NIM-giver's live [`SwapSigner`]: funds the real NIM HTLC at `Accepted`, lands the real
/// Amoy `withdraw(S)` at `BothFunded` (the on-chain reveal), never touches any other leg.
pub struct LiveInitiatorSigner {
    cfg: LiveInitiatorConfig,
    clock_ms: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The last successfully-landed claim, replayed if the driver retries after a poll timeout.
    last_claim: Mutex<Option<(Vec<u8>, [u8; 32])>>,
}

impl LiveInitiatorSigner {
    /// A live initiator signer over `cfg`.
    pub fn new(cfg: LiveInitiatorConfig) -> Self {
        LiveInitiatorSigner {
            cfg,
            clock_ms: Box::new(system_now_ms),
            last_claim: Mutex::new(None),
        }
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock_ms: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock_ms = clock_ms;
        self
    }

    /// Build + broadcast the REAL NIM HTLC creation for this swap: recipient = the responder's
    /// protocol-carried NIM claim address, timeout = the ADR-0010 ms mapping of `T_A`, value =
    /// `give_amount` luna, funder/refunder = our NIM key. Returns `(wire, tx_hash)` once the
    /// node accepted the broadcast (inclusion is the RESPONDER's verifier's job).
    fn fund_nim(&self, ctx: &SwapContext) -> Option<(Vec<u8>, [u8; 32])> {
        if ctx.network_id != crate::NetworkId::Testnet.wire_id() {
            eprintln!("[live-swap] fund_nim: refused non-testnet network {}", {
                ctx.network_id
            });
            return None;
        }
        let peer = self.cfg.peer_book.get(&ctx.swap_id)?;
        let head = match self.cfg.nim_rpc.block_number() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[live-swap] fund_nim: head fetch failed: {e}");
                return None;
            }
        };
        let funder_pk: [u8; 32] = self.cfg.nim_key.public_key().try_into().ok()?;
        let funder = Address::from_public_key(&funder_pk);
        let creation = HtlcCreation {
            funder,
            data: HtlcCreationData {
                htlc_sender: funder, // refund → back to our funded wallet after T_A
                htlc_recipient: Address::from_bytes(peer.nim_address),
                hash_algorithm: HashAlgorithm::Sha256,
                hash_root: ctx.hashlock,
                hash_count: 1,
                timeout: nim_htlc_timeout_ms(
                    ctx.terms.nim_timeout,
                    self.cfg.term_anchor,
                    (self.clock_ms)(),
                ),
            },
            value: ctx.give_amount,
            fee: 0,
            validity_start_height: head,
            network_id: ctx.network_id,
        };
        let signature: [u8; 64] = self
            .cfg
            .nim_key
            .sign_content(creation.serialize_content())
            .try_into()
            .ok()?;
        let wire = creation.serialize_wire(&signature_proof_single_sig(&funder_pk, &signature));
        let tx_hash = creation.tx_hash();
        if !broadcast_nim(&self.cfg.nim_rpc, &wire, &tx_hash, "fund_nim") {
            return None;
        }
        eprintln!(
            "[live-swap] fund_nim: NIM HTLC {} funded ({} luna, tx {})",
            creation.contract_address().to_user_friendly(),
            ctx.give_amount,
            bytes_to_hex(&tx_hash)
        );
        Some((wire, tx_hash))
    }

    /// Land the REAL Amoy `withdraw(S)` on the escrow this node's verifier found (the on-chain
    /// reveal). Only a MINED, successful withdraw returns `Some` — the wire is `S ‖ raw tx`,
    /// so the responder reads `S` off its first 32 bytes.
    fn claim_usdc(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        let escrow = self.cfg.store.found(&ctx.hashlock)?;
        // A prior attempt that timed out at the receipt poll may have landed after all —
        // replay it rather than double-spending the nonce.
        if let Some((wire, id)) = self.last_claim.lock().unwrap().clone() {
            if escrow_state(&self.cfg.amoy, &self.cfg.htlc, &escrow.swap_id) == Some(STATE_CLAIMED)
            {
                return Some((wire, id));
            }
        }
        let me = self.cfg.evm_gas_key.address();
        let gas_price = self
            .cfg
            .amoy
            .gas_price()
            .ok()?
            .clamp(self.cfg.gas.min_gas_price, self.cfg.gas.max_gas_price);
        let nonce = self.cfg.amoy.transaction_count(&me).ok()?;
        let (raw, hash) = send_evm_tx(
            &self.cfg.amoy,
            &self.cfg.evm_gas_key,
            nonce,
            gas_price,
            self.cfg.gas.gas_withdraw,
            &self.cfg.htlc,
            &htlc_withdraw(&escrow.swap_id, &secret),
            "withdraw",
        )?;
        let receipt = wait_receipt(&self.cfg.amoy, &hash, &self.cfg.poll, "withdraw")?;
        if !receipt.success {
            eprintln!("[live-swap] withdraw REVERTED — S was NOT revealed");
            return None;
        }
        eprintln!(
            "[live-swap] withdraw(S) mined in block {} (tx 0x{})",
            receipt.block_number,
            bytes_to_hex(&hash)
        );
        let mut wire = secret.to_vec(); // S first — the responder reads it off the reveal
        wire.extend_from_slice(&raw);
        *self.last_claim.lock().unwrap() = Some((wire.clone(), hash));
        Some((wire, hash))
    }
}

impl SwapSigner for LiveInitiatorSigner {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        match leg {
            SwapLegId::Nim => self.fund_nim(ctx),
            SwapLegId::Counterparty => None, // the responder's leg — never ours to fund
        }
    }

    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        self.claim_usdc(ctx, secret)
    }

    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.cfg.peer_book.note(
            swap_id,
            PeerAddressing {
                nim_address: peer_nim_address,
                chain_address: peer_chain_address.to_vec(),
            },
        );
    }
}

/// Broadcast a signed NIM wire; treat "the node already knows this tx" as success.
fn broadcast_nim(rpc: &Arc<dyn GatewayRpc>, wire: &[u8], tx_hash: &[u8; 32], label: &str) -> bool {
    match rpc.send_raw_transaction(&bytes_to_hex(wire)) {
        Ok(_) => true,
        Err(e) => {
            if matches!(rpc.get_transaction(&bytes_to_hex(tx_hash)), Ok(Some(_))) {
                return true; // a prior attempt landed it
            }
            eprintln!("[live-swap] {label}: NIM broadcast failed: {e}");
            false
        }
    }
}

// --- the responder signer ---------------------------------------------------------------------

/// Everything the USDC-giver's live signer needs at construction.
pub struct LiveResponderConfig {
    /// The funded Amoy account (escrows the USDC + pays its own gas).
    pub evm_key: LocalEvmKey,
    /// The Amoy chain client (guarded).
    pub amoy: Arc<dyn AmoyChain>,
    /// The deployed `NimmeshHtlc`.
    pub htlc: EvmAddress,
    /// The USDC token the HTLC escrows.
    pub usdc: EvmAddress,
    /// Shared peer-addressing sink (the initiator's EVM claim address arrives via `Propose`).
    pub peer_book: Arc<PeerBook>,
    /// The key owning this session's NIM claim address (`identity.nim_address`).
    pub nim_claim_key: Arc<dyn EnclaveKey>,
    /// The testnet-guarded Albatross RPC.
    pub nim_rpc: Arc<dyn GatewayRpc>,
    /// Shared with this node's [`crate::nim_verifier::NimHtlcVerifier`] — the claim reuses the
    /// exact contract the verifier confirmed on-chain.
    pub nim_store: Arc<NimFundingStore>,
    /// Gas policy.
    pub gas: EvmGasConfig,
    /// Blocking-wait policy.
    pub poll: LivePollConfig,
    /// The mesh-head anchor the session terms are relative to (ADR-0010).
    pub term_anchor: u64,
}

/// The USDC-giver's live [`SwapSigner`]: lands `approve` + `newSwap` at `InitiatorFunded`
/// (after ITS verifier confirmed the NIM leg), claims the NIM HTLC with `S` at `Revealed`.
pub struct LiveResponderSigner {
    cfg: LiveResponderConfig,
    clock_s: Box<dyn Fn() -> u64 + Send + Sync>,
    /// The last successfully-landed funding (wire + tx id + swapId), replayed if the driver
    /// retries after a partial failure and the escrow turns out live after all.
    last_funding: Mutex<Option<LandedFunding>>,
}

impl LiveResponderSigner {
    /// A live responder signer over `cfg`.
    pub fn new(cfg: LiveResponderConfig) -> Self {
        LiveResponderSigner {
            cfg,
            clock_s: Box::new(system_now_s),
            last_funding: Mutex::new(None),
        }
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock_s: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock_s = clock_s;
        self
    }

    /// Escrow the REAL USDC on Amoy: `approve(htlc, amount)` then `newSwap(receiver =
    /// the initiator's protocol-carried EVM claim address, take_amount micro-USDC, H,
    /// timelock = the ADR-0010 seconds mapping of `T_B`)`. Both txs must MINE successfully;
    /// the returned wire is the raw `newSwap` tx (what the initiator's verifier anchors on).
    fn fund_usdc(&self, ctx: &SwapContext) -> Option<(Vec<u8>, [u8; 32])> {
        // A prior attempt may have landed the escrow even though we reported failure (e.g. a
        // receipt-poll timeout). If it is live on-chain, replay it instead of re-escrowing.
        if let Some((wire, id, swap_id)) = self.last_funding.lock().unwrap().clone() {
            if matches!(
                escrow_state(&self.cfg.amoy, &self.cfg.htlc, &swap_id),
                Some(STATE_LIVE) | Some(STATE_CLAIMED)
            ) {
                return Some((wire, id));
            }
        }
        let peer = self.cfg.peer_book.get(&ctx.swap_id)?;
        let receiver: EvmAddress = match peer.chain_address.as_slice().try_into() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("[live-swap] fund_usdc: the peer's chain address is not 20 bytes");
                return None;
            }
        };
        let me = self.cfg.evm_key.address();
        let amount = ctx.take_amount; // micro-USDC for this pair (ADR-0010)
        let timelock_s = usdc_timelock_s(
            ctx.terms.counterparty_timeout,
            self.cfg.term_anchor,
            (self.clock_s)(),
        );
        let gas_price = self
            .cfg
            .amoy
            .gas_price()
            .ok()?
            .clamp(self.cfg.gas.min_gas_price, self.cfg.gas.max_gas_price);
        // Budget preflight (AMOY.md): the node checks `limit × price` per tx up front, and the
        // refund reserve must survive — never strand an escrow because the refund can't be paid.
        let budget = (self.cfg.gas.gas_approve
            + self.cfg.gas.gas_new_swap
            + self.cfg.gas.gas_refund_reserve)
            .saturating_mul(gas_price);
        match self.cfg.amoy.balance(&me) {
            Ok(pol) if pol >= u128::from(budget) => {}
            Ok(pol) => {
                eprintln!("[live-swap] fund_usdc: POL budget short ({pol} < {budget} wei)");
                return None;
            }
            Err(e) => {
                eprintln!("[live-swap] fund_usdc: balance preflight failed: {e}");
                return None;
            }
        }
        let mut nonce = self.cfg.amoy.transaction_count(&me).ok()?;

        let (_, approve_hash) = send_evm_tx(
            &self.cfg.amoy,
            &self.cfg.evm_key,
            nonce,
            gas_price,
            self.cfg.gas.gas_approve,
            &self.cfg.usdc,
            &erc20_approve(&self.cfg.htlc, amount),
            "approve",
        )?;
        nonce += 1;
        if !wait_receipt(&self.cfg.amoy, &approve_hash, &self.cfg.poll, "approve")?.success {
            eprintln!("[live-swap] approve REVERTED");
            return None;
        }

        let (raw, new_swap_hash) = send_evm_tx(
            &self.cfg.amoy,
            &self.cfg.evm_key,
            nonce,
            gas_price,
            self.cfg.gas.gas_new_swap,
            &self.cfg.htlc,
            &htlc_new_swap(&receiver, amount, &ctx.hashlock, timelock_s),
            "newSwap",
        )?;
        let receipt = wait_receipt(&self.cfg.amoy, &new_swap_hash, &self.cfg.poll, "newSwap")?;
        if !receipt.success {
            eprintln!("[live-swap] newSwap REVERTED");
            return None;
        }
        let swap_id = usdc_swap_id(&me, &receiver, amount, &ctx.hashlock, timelock_s);
        eprintln!(
            "[live-swap] fund_usdc: escrow live (swapId 0x{}, {} micro-USDC, block {})",
            bytes_to_hex(&swap_id),
            amount,
            receipt.block_number
        );
        *self.last_funding.lock().unwrap() = Some((raw.clone(), new_swap_hash, swap_id));
        Some((raw, new_swap_hash))
    }

    /// Claim the REAL NIM HTLC with the revealed `S`: redeem the exact contract this node's
    /// verifier confirmed (shared [`NimFundingStore`]) to our own NIM claim address.
    fn claim_nim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        let rec = self.cfg.nim_store.get(&ctx.hashlock)?;
        let claim_pk: [u8; 32] = self.cfg.nim_claim_key.public_key().try_into().ok()?;
        // The redeem pays the session's NIM identity — which this key must actually own.
        if Address::from_public_key(&claim_pk).as_bytes() != &ctx.nim_address {
            eprintln!("[live-swap] claim_nim: claim key does not own the session NIM address");
            return None;
        }
        let head = match self.cfg.nim_rpc.block_number() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[live-swap] claim_nim: head fetch failed: {e}");
                return None;
            }
        };
        let redeem = HtlcRedeem {
            contract: rec.contract,
            recipient: Address::from_bytes(ctx.nim_address),
            value: rec.creation.value,
            fee: 0,
            validity_start_height: head,
            network_id: ctx.network_id,
        };
        let signature: [u8; 64] = self
            .cfg
            .nim_claim_key
            .sign_content(redeem.serialize_content())
            .try_into()
            .ok()?;
        let proof = regular_transfer_proof(
            HashAlgorithm::Sha256,
            &rec.creation.data.hash_root,
            &secret,
            &claim_pk,
            &signature,
        );
        let wire = redeem.serialize_wire(&proof);
        let tx_hash = redeem.tx_hash();
        if !broadcast_nim(&self.cfg.nim_rpc, &wire, &tx_hash, "claim_nim") {
            return None;
        }
        eprintln!(
            "[live-swap] claim_nim: claimed {} luna from {} (tx {})",
            rec.creation.value,
            rec.contract.to_user_friendly(),
            bytes_to_hex(&tx_hash)
        );
        Some((wire, tx_hash))
    }
}

impl SwapSigner for LiveResponderSigner {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        match leg {
            SwapLegId::Counterparty => self.fund_usdc(ctx),
            SwapLegId::Nim => None, // the initiator's leg — never ours to fund
        }
    }

    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        self.claim_nim(ctx, secret)
    }

    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.cfg.peer_book.note(
            swap_id,
            PeerAddressing {
                nim_address: peer_nim_address,
                chain_address: peer_chain_address.to_vec(),
            },
        );
    }
}

#[cfg(test)]
#[path = "live_swap_signer_tests.rs"]
mod tests;
