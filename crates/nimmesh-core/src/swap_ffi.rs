//! # swap_ffi — the UniFFI facade over [`crate::swap_engine::SwapEngine`] (`bitcoin-leg` feature)
//!
//! Exposes the cross-chain swap engine across the FFI boundary so the WebView UI (and the native
//! mesh shim) can drive a NIM⇄BTC swap, exactly as the NIM signer ([`crate::nimiq::signer::AppSigner`])
//! is exposed. The pure [`SwapEngine`] stays uniffi-free; this facade owns the interior mutability
//! (`Mutex`), the bitcoin-type ↔ FFI conversions (txids/addresses as strings, network as an enum),
//! and flattens [`crate::swap_engine::EngineError`] to a stringly error the bindings can surface.
//!
//! The two enclave keys cross as the same `with_foreign` traits the signer uses
//! ([`EnclaveKey`] / [`crate::btc::BtcEnclaveKey`]) — the seeds never cross FFI, only a public key
//! and a signature do. Testnet/signet only; mainnet is not offered here.

use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use bitcoin::{Address as BtcAddress, Network, Txid};

use crate::btc::{BtcEnclaveKey, FundedHtlc};
use crate::nimiq::signer::EnclaveKey;
use crate::swap::{LadderParams, SwapPhase, SwapRole, SwapTerms};
use crate::swap_btc_leg::BtcSwapLeg;
use crate::swap_builder::NimiqLeg;
use crate::swap_engine::{EngineError, SwapConfig, SwapEffect, SwapEngine};
use crate::swap_wire::SwapLegId;

/// The BTC network a swap runs on. Mainnet is intentionally absent (gated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiBtcNetwork {
    /// Bitcoin testnet3 (`tb1…`).
    Testnet,
    /// Bitcoin signet (`tb1…`, same params, different coins).
    Signet,
}

impl From<FfiBtcNetwork> for Network {
    fn from(n: FfiBtcNetwork) -> Self {
        match n {
            FfiBtcNetwork::Testnet => Network::Testnet,
            FfiBtcNetwork::Signet => Network::Signet,
        }
    }
}

/// Tunable timelock-ladder thresholds, in the engine's Unix-**ms** base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FfiLadder {
    /// Minimum required `T_A − T_B` (ms).
    pub delta_safe_ms: u64,
    /// Minimum required `T_B − head` before funding (ms).
    pub min_window_ms: u64,
}

impl From<FfiLadder> for LadderParams {
    fn from(l: FfiLadder) -> Self {
        LadderParams {
            delta_safe_blocks: l.delta_safe_ms,
            min_claim_window_blocks: l.min_window_ms,
        }
    }
}

/// Which chain a leg refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiLeg {
    /// The Nimiq leg.
    Nim,
    /// The counterparty (Bitcoin) leg.
    Counterparty,
}

impl From<SwapLegId> for FfiLeg {
    fn from(l: SwapLegId) -> Self {
        match l {
            SwapLegId::Nim => FfiLeg::Nim,
            SwapLegId::Counterparty => FfiLeg::Counterparty,
        }
    }
}

/// The swap lifecycle phase, FFI mirror of [`SwapPhase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSwapPhase {
    /// Terms on the table.
    Proposed,
    /// Agreed; ladder checked safe.
    Accepted,
    /// (Responder) saw the initiator fund.
    InitiatorFunded,
    /// This node funded its leg.
    SelfFunded,
    /// Both legs funded.
    BothFunded,
    /// The secret is out.
    Revealed,
    /// This node's claim settled (terminal success).
    Settled,
    /// Cancelled before funding (terminal).
    Aborted,
    /// Refunded via timeout (terminal).
    Refunded,
}

impl From<SwapPhase> for FfiSwapPhase {
    fn from(p: SwapPhase) -> Self {
        match p {
            SwapPhase::Proposed => FfiSwapPhase::Proposed,
            SwapPhase::Accepted => FfiSwapPhase::Accepted,
            SwapPhase::InitiatorFunded => FfiSwapPhase::InitiatorFunded,
            SwapPhase::SelfFunded => FfiSwapPhase::SelfFunded,
            SwapPhase::BothFunded => FfiSwapPhase::BothFunded,
            SwapPhase::Revealed => FfiSwapPhase::Revealed,
            SwapPhase::Settled => FfiSwapPhase::Settled,
            SwapPhase::Aborted => FfiSwapPhase::Aborted,
            SwapPhase::Refunded => FfiSwapPhase::Refunded,
        }
    }
}

/// Which side this node plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSwapRole {
    /// Gives NIM, wants BTC, knows the secret.
    Initiator,
    /// Gives BTC, wants NIM, learns the secret from the BTC claim.
    Responder,
}

impl From<SwapRole> for FfiSwapRole {
    fn from(r: SwapRole) -> Self {
        match r {
            SwapRole::Initiator => FfiSwapRole::Initiator,
            SwapRole::Responder => FfiSwapRole::Responder,
        }
    }
}

/// What the caller must do on-chain after a transition.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSwapEffect {
    /// Broadcast these signed tx bytes on the named leg.
    Broadcast {
        /// Which chain.
        leg: FfiLeg,
        /// The signed tx bytes.
        tx: Vec<u8>,
    },
    /// Pay `amount_sat` to this BTC P2WSH HTLC address from the wallet.
    FundBtcAddress {
        /// The P2WSH address.
        address: String,
        /// Amount in satoshis.
        amount_sat: u64,
    },
}

impl From<SwapEffect> for FfiSwapEffect {
    fn from(e: SwapEffect) -> Self {
        match e {
            SwapEffect::Broadcast { leg, tx } => FfiSwapEffect::Broadcast {
                leg: leg.into(),
                tx,
            },
            SwapEffect::FundBtcAddress {
                address,
                amount_sat,
            } => FfiSwapEffect::FundBtcAddress {
                address,
                amount_sat,
            },
        }
    }
}

/// The agreed, public swap parameters needed to construct an engine (both sides supply the same
/// hashlock, pubkeys, timeouts, and amounts; each supplies its **own** BTC payout address + key).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSwapParams {
    /// `T_A` — the NIM-leg timeout, a Unix-**ms** timestamp (the longer one).
    pub nim_timeout_ms: u64,
    /// `T_B` — the BTC-leg timeout, a Unix-**ms** timestamp (`T_B < T_A`). The BTC CLTV is `T_B/1000`.
    pub counterparty_timeout_ms: u64,
    /// `H = SHA-256(secret)` — 32 bytes, the shared hashlock.
    pub hashlock: Vec<u8>,
    /// The initiator's 33-byte compressed BTC pubkey (the claimant of the BTC HTLC).
    pub claimant_btc_pubkey: Vec<u8>,
    /// The responder's 33-byte compressed BTC pubkey (the funder/refunder of the BTC HTLC).
    pub funder_btc_pubkey: Vec<u8>,
    /// The BTC network (testnet/signet).
    pub network: FfiBtcNetwork,
    /// This node's BTC payout address (where its claimed/refunded BTC lands).
    pub btc_payout_address: String,
    /// The flat BTC fee, in satoshis.
    pub btc_fee_sat: u64,
    /// Luna locked on the NIM leg.
    pub nim_amount: u64,
    /// Sat locked on the BTC leg.
    pub btc_amount_sat: u64,
    /// The 20-byte raw NIM address that claims the NIM leg (the responder's). Empty for the responder.
    pub counterparty_nim_address: Vec<u8>,
}

/// A failure across the swap FFI surface.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum SwapEngineError {
    /// The engine/state-machine rejected the action (illegal phase, unsafe ladder, bad preimage…).
    Rejected {
        /// A human-readable reason.
        reason: String,
    },
    /// A provided input was malformed (wrong-length key, bad address/txid…).
    BadInput {
        /// A human-readable reason.
        reason: String,
    },
}

impl std::fmt::Display for SwapEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapEngineError::Rejected { reason } => write!(f, "swap rejected: {reason}"),
            SwapEngineError::BadInput { reason } => write!(f, "bad input: {reason}"),
        }
    }
}

impl std::error::Error for SwapEngineError {}

impl From<EngineError> for SwapEngineError {
    fn from(e: EngineError) -> Self {
        SwapEngineError::Rejected {
            reason: e.to_string(),
        }
    }
}

fn fixed<const N: usize>(v: &[u8], what: &str) -> Result<[u8; N], SwapEngineError> {
    v.try_into().map_err(|_| SwapEngineError::BadInput {
        reason: format!("{what} must be {N} bytes, got {}", v.len()),
    })
}

fn parse_funded(txid: &str, vout: u32, value_sat: u64) -> Result<FundedHtlc, SwapEngineError> {
    Ok(FundedHtlc {
        txid: Txid::from_str(txid).map_err(|e| SwapEngineError::BadInput {
            reason: format!("bad txid: {e}"),
        })?,
        vout,
        value_sat,
    })
}

fn build_config(
    params: FfiSwapParams,
    nim_key: Arc<dyn EnclaveKey>,
    btc_key: Arc<dyn BtcEnclaveKey>,
) -> Result<SwapConfig, SwapEngineError> {
    let network: Network = params.network.into();
    let payout = BtcAddress::from_str(&params.btc_payout_address)
        .map_err(|e| SwapEngineError::BadInput {
            reason: format!("bad BTC payout address: {e}"),
        })?
        .require_network(network)
        .map_err(|e| SwapEngineError::BadInput {
            reason: format!("payout address not valid on {network:?}: {e}"),
        })?;
    let btc = BtcSwapLeg::new(
        fixed(&params.hashlock, "hashlock")?,
        fixed(&params.claimant_btc_pubkey, "claimant pubkey")?,
        fixed(&params.funder_btc_pubkey, "funder pubkey")?,
        (params.counterparty_timeout_ms / 1000) as i64, // BTC CLTV = T_B in seconds
        network,
        btc_key,
        payout.script_pubkey(),
        params.btc_fee_sat,
    );
    Ok(SwapConfig {
        terms: SwapTerms {
            nim_timeout: params.nim_timeout_ms,
            counterparty_timeout: params.counterparty_timeout_ms,
        },
        nim: NimiqLeg::new(nim_key),
        btc,
        nim_amount: params.nim_amount,
        btc_amount_sat: params.btc_amount_sat,
        counterparty_nim_address: params.counterparty_nim_address,
    })
}

/// The FFI handle to one node's cross-chain swap. Construct with [`new_initiator`] /
/// [`new_responder`], then drive it (accept → fund → observe → claim/refund). Thread-safe: the inner
/// engine is behind a `Mutex` so the UI/mesh can hold an `Arc` and call from anywhere.
///
/// [`new_initiator`]: SwapEngineHandle::new_initiator
/// [`new_responder`]: SwapEngineHandle::new_responder
#[derive(uniffi::Object)]
pub struct SwapEngineHandle {
    inner: Mutex<SwapEngine>,
}

impl SwapEngineHandle {
    fn lock(&self) -> MutexGuard<'_, SwapEngine> {
        self.inner.lock().expect("swap engine mutex poisoned")
    }
}

#[uniffi::export]
impl SwapEngineHandle {
    /// The **initiator** (gives NIM, wants BTC). `secret` is the 32-byte swap preimage.
    #[uniffi::constructor]
    pub fn new_initiator(
        params: FfiSwapParams,
        nim_key: Arc<dyn EnclaveKey>,
        btc_key: Arc<dyn BtcEnclaveKey>,
        secret: Vec<u8>,
    ) -> Result<Arc<Self>, SwapEngineError> {
        let secret = fixed(&secret, "secret")?;
        let config = build_config(params, nim_key, btc_key)?;
        Ok(Arc::new(SwapEngineHandle {
            inner: Mutex::new(SwapEngine::new_initiator(config, secret)),
        }))
    }

    /// The **responder** (gives BTC, wants NIM; learns the secret from the BTC claim).
    #[uniffi::constructor]
    pub fn new_responder(
        params: FfiSwapParams,
        nim_key: Arc<dyn EnclaveKey>,
        btc_key: Arc<dyn BtcEnclaveKey>,
    ) -> Result<Arc<Self>, SwapEngineError> {
        let config = build_config(params, nim_key, btc_key)?;
        Ok(Arc::new(SwapEngineHandle {
            inner: Mutex::new(SwapEngine::new_responder(config)),
        }))
    }

    /// The current lifecycle phase.
    pub fn phase(&self) -> FfiSwapPhase {
        self.lock().phase().into()
    }

    /// Which side this node plays.
    pub fn role(&self) -> FfiSwapRole {
        self.lock().role().into()
    }

    /// The shared hashlock `H` (32 bytes).
    pub fn hashlock(&self) -> Vec<u8> {
        self.lock().hashlock().to_vec()
    }

    /// The BTC P2WSH HTLC address (to fund / watch).
    pub fn btc_htlc_address(&self) -> String {
        self.lock().btc_htlc_address()
    }

    /// Move `Proposed → Accepted`, refusing an unsafe ladder at `head_ms`.
    pub fn accept(&self, head_ms: u64, ladder: FfiLadder) -> Result<(), SwapEngineError> {
        self.lock()
            .accept(head_ms, &ladder.into())
            .map_err(Into::into)
    }

    /// (Responder) record the observed initiator NIM-HTLC funding tx → `InitiatorFunded`.
    pub fn observe_initiator_funded(
        &self,
        nim_funding_wire: Vec<u8>,
    ) -> Result<(), SwapEngineError> {
        self.lock()
            .observe_initiator_funded(nim_funding_wire)
            .map_err(Into::into)
    }

    /// Fund this node's own leg. `vsh` anchors a NIM funding (ignored for BTC).
    pub fn fund(
        &self,
        head_ms: u64,
        ladder: FfiLadder,
        vsh: u32,
    ) -> Result<FfiSwapEffect, SwapEngineError> {
        self.lock()
            .fund(head_ms, &ladder.into(), vsh)
            .map(Into::into)
            .map_err(Into::into)
    }

    /// (Initiator) record the observed BTC HTLC funding output → `BothFunded`.
    pub fn observe_btc_funded(
        &self,
        txid: String,
        vout: u32,
        value_sat: u64,
    ) -> Result<(), SwapEngineError> {
        let funded = parse_funded(&txid, vout, value_sat)?;
        self.lock().observe_btc_funded(funded).map_err(Into::into)
    }

    /// (Responder) confirm both legs funded → `BothFunded`, recording this node's own BTC outpoint.
    pub fn observe_nim_funded(
        &self,
        txid: String,
        vout: u32,
        value_sat: u64,
    ) -> Result<(), SwapEngineError> {
        let funded = parse_funded(&txid, vout, value_sat)?;
        self.lock().observe_nim_funded(funded).map_err(Into::into)
    }

    /// (Initiator) claim the BTC leg, revealing `S` on-chain → a BTC claim tx to broadcast.
    pub fn reveal_and_claim_btc(&self) -> Result<FfiSwapEffect, SwapEngineError> {
        self.lock()
            .reveal_and_claim_btc()
            .map(Into::into)
            .map_err(Into::into)
    }

    /// (Responder) read `S` off the initiator's BTC claim and claim the NIM leg → a NIM claim tx.
    pub fn claim_nim_from_btc_claim(
        &self,
        btc_claim_tx: Vec<u8>,
        vsh: u32,
    ) -> Result<FfiSwapEffect, SwapEngineError> {
        self.lock()
            .claim_nim_from_btc_claim(&btc_claim_tx, vsh)
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Record that this node's claim settled → `Settled`.
    pub fn observe_settled(&self) -> Result<(), SwapEngineError> {
        self.lock().observe_settled().map_err(Into::into)
    }

    /// Cancel before funding → `Aborted`.
    pub fn abort(&self) -> Result<(), SwapEngineError> {
        self.lock().abort().map_err(Into::into)
    }

    /// Refund this node's own funded leg via the timeout path (once past the leg's timeout).
    pub fn refund(&self, head_ms: u64, vsh: u32) -> Result<FfiSwapEffect, SwapEngineError> {
        self.lock()
            .refund(head_ms, vsh)
            .map(Into::into)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*; // brings EnclaveKey + BtcEnclaveKey traits into scope for `.public_key()`
    use crate::btc::InMemoryBtcEnclaveKey;
    use crate::nimiq::address::Address as NimAddress;
    use crate::nimiq::signer::InMemoryEnclaveKey;
    use bitcoin::{CompressedPublicKey, Network};

    const HEAD_MS: u64 = 1_000_000_000_000;
    const T_B_MS: u64 = HEAD_MS + 3_600_000;
    const T_A_MS: u64 = HEAD_MS + 7_200_000;

    fn ladder() -> FfiLadder {
        FfiLadder {
            delta_safe_ms: 1_800_000,
            min_window_ms: 1_800_000,
        }
    }

    fn btc_pk(seed: u8) -> Vec<u8> {
        InMemoryBtcEnclaveKey::from_secret(&[seed; 32])
            .unwrap()
            .public_key()
    }

    fn btc_payout(seed: u8) -> String {
        let pk = btc_pk(seed);
        BtcAddress::p2wpkh(
            &CompressedPublicKey::from_slice(&pk).unwrap(),
            Network::Signet,
        )
        .to_string()
    }

    fn nim_addr(seed: u8) -> Vec<u8> {
        let pk: [u8; 32] = InMemoryEnclaveKey::from_secret(&[seed; 32])
            .public_key()
            .try_into()
            .unwrap();
        NimAddress::from_public_key(&pk).as_bytes().to_vec()
    }

    fn params(payout_seed: u8, counterparty_nim: Vec<u8>) -> FfiSwapParams {
        FfiSwapParams {
            nim_timeout_ms: T_A_MS,
            counterparty_timeout_ms: T_B_MS,
            hashlock: crate::swap_leg::sha256(&[0x42u8; 32]).to_vec(),
            claimant_btc_pubkey: btc_pk(0x11),
            funder_btc_pubkey: btc_pk(0x22),
            network: FfiBtcNetwork::Signet,
            btc_payout_address: btc_payout(payout_seed),
            btc_fee_sat: 500,
            nim_amount: 200_000,
            btc_amount_sat: 100_000,
            counterparty_nim_address: counterparty_nim,
        }
    }

    fn nim_key(seed: u8) -> Arc<dyn EnclaveKey> {
        Arc::new(InMemoryEnclaveKey::from_secret(&[seed; 32]))
    }
    fn btc_key(seed: u8) -> Arc<dyn BtcEnclaveKey> {
        Arc::new(InMemoryBtcEnclaveKey::from_secret(&[seed; 32]).unwrap())
    }

    #[test]
    fn ffi_handle_drives_a_swap_across_the_boundary() {
        // Initiator funds NIM; responder is handed the BTC address to pay — both through the FFI types.
        let init = SwapEngineHandle::new_initiator(
            params(0x11, nim_addr(4)),
            nim_key(3),
            btc_key(0x11),
            vec![0x42u8; 32],
        )
        .unwrap();
        let resp = SwapEngineHandle::new_responder(params(0x22, vec![]), nim_key(4), btc_key(0x22))
            .unwrap();

        assert_eq!(init.role(), FfiSwapRole::Initiator);
        assert_eq!(init.phase(), FfiSwapPhase::Proposed);
        assert_eq!(init.hashlock().len(), 32);

        init.accept(HEAD_MS, ladder()).unwrap();
        resp.accept(HEAD_MS, ladder()).unwrap();

        let nim_wire = match init.fund(HEAD_MS, ladder(), 100).unwrap() {
            FfiSwapEffect::Broadcast {
                leg: FfiLeg::Nim,
                tx,
            } => {
                assert_eq!(tx.len(), 248);
                tx
            }
            other => panic!("expected NIM broadcast, got {other:?}"),
        };

        resp.observe_initiator_funded(nim_wire).unwrap();
        match resp.fund(HEAD_MS, ladder(), 100).unwrap() {
            FfiSwapEffect::FundBtcAddress {
                address,
                amount_sat,
            } => {
                assert_eq!(amount_sat, 100_000);
                // Both sides derive the same P2WSH from the shared params.
                assert_eq!(address, init.btc_htlc_address());
            }
            other => panic!("expected a BTC address, got {other:?}"),
        }
        assert_eq!(resp.phase(), FfiSwapPhase::SelfFunded);
    }

    #[test]
    fn ffi_constructor_rejects_a_bad_pubkey_length() {
        let mut p = params(0x11, vec![]);
        p.claimant_btc_pubkey = vec![0u8; 10]; // not 33 bytes
                                               // Match rather than `expect_err` — the Ok type (`Arc<SwapEngineHandle>`) has no `Debug`.
        match SwapEngineHandle::new_responder(p, nim_key(4), btc_key(0x22)) {
            Err(SwapEngineError::BadInput { .. }) => {}
            Err(SwapEngineError::Rejected { reason }) => panic!("wrong error: {reason}"),
            Ok(_) => panic!("must reject a bad pubkey length"),
        }
    }

    #[test]
    fn ffi_accept_surfaces_an_unsafe_ladder_as_rejected() {
        // Inverted ladder via swapped timeouts.
        let mut p = params(0x11, nim_addr(4));
        p.nim_timeout_ms = T_B_MS;
        p.counterparty_timeout_ms = T_A_MS;
        let init = SwapEngineHandle::new_initiator(p, nim_key(3), btc_key(0x11), vec![0x42u8; 32])
            .unwrap();
        let err = init.accept(HEAD_MS, ladder()).expect_err("unsafe ladder");
        assert!(matches!(err, SwapEngineError::Rejected { .. }));
        assert_eq!(init.phase(), FfiSwapPhase::Proposed);
    }
}
