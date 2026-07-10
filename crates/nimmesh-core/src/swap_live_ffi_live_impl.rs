//! # swap_live_ffi::live_impl — the featured guts of the live swap door (behind
//! `polygon-gateway` + `gateway-rpc`), split from `swap_live_ffi.rs` for the 800-line guard.
//! Included via `#[path]` from that module, so its `super` is `swap_live_ffi` and every
//! `pub(crate)` item stays reachable through `swap_live_ffi::live_impl::…` (the tests do).

use super::*;
use crate::swap_wire::NIM_ADDRESS_LEN;

use crate::nimiq::htlc::decode_creation_wire;
use crate::swap_coordinator::SwapContext;
use crate::swap_signer::SwapSigner;
use crate::swap_wire::{SwapLegId, SWAP_ID_LEN};

// --- the signer wrappers (latch + lock recording) --------------------------------------------

/// The A2c one-shot funding latch, now in the production door: after the FIRST successful
/// funding, every further funding is refused (claims and peer notes pass through) — one
/// construction can never move more than one advertised trade (module docs).
pub(crate) struct OneShotSigner<S: SwapSigner> {
    inner: S,
    fired: std::sync::atomic::AtomicBool,
}

impl<S: SwapSigner> OneShotSigner<S> {
    pub(crate) fn new(inner: S) -> Self {
        OneShotSigner {
            inner,
            fired: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl<S: SwapSigner> SwapSigner for OneShotSigner<S> {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        use std::sync::atomic::Ordering;
        if self.fired.load(Ordering::SeqCst) {
            return None; // one real funding per construction — a repeat match stays unfunded
        }
        let out = self.inner.build_funding(ctx, leg);
        if out.is_some() {
            self.fired.store(true, Ordering::SeqCst);
        }
        out
    }
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        self.inner.build_claim(ctx, secret)
    }
    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.inner
            .note_peer(swap_id, peer_nim_address, peer_chain_address);
    }
    fn is_live(&self) -> bool {
        self.inner.is_live() // C1: a wrapper must never hide a live signer
    }
}

/// Records every NIM funding the wrapped signer actually lands into the caller's
/// [`LiveLockBook`] — decoded from the exact wire that was broadcast, so the book can never
/// disagree with the chain about what was locked.
pub(crate) struct LockRecordingSigner<S: SwapSigner> {
    inner: S,
    book: Arc<LiveLockBook>,
}

impl<S: SwapSigner> LockRecordingSigner<S> {
    pub(crate) fn new(inner: S, book: Arc<LiveLockBook>) -> Self {
        LockRecordingSigner { inner, book }
    }
}

impl<S: SwapSigner> SwapSigner for LockRecordingSigner<S> {
    fn build_funding(&self, ctx: &SwapContext, leg: SwapLegId) -> Option<(Vec<u8>, [u8; 32])> {
        let out = self.inner.build_funding(ctx, leg);
        if leg == SwapLegId::Nim {
            if let Some((wire, _)) = &out {
                if let Some(creation) = decode_creation_wire(wire) {
                    self.book.record(FfiNimLock {
                        contract: creation.contract_address().to_user_friendly(),
                        value: creation.value,
                        timeout_ms: creation.data.timeout,
                        funding_tx_hash: bytes_to_hex(&creation.tx_hash()),
                    });
                }
            }
        }
        out
    }
    fn build_claim(&self, ctx: &SwapContext, secret: [u8; 32]) -> Option<(Vec<u8>, [u8; 32])> {
        self.inner.build_claim(ctx, secret)
    }
    fn note_peer(
        &self,
        swap_id: [u8; SWAP_ID_LEN],
        peer_nim_address: [u8; NIM_ADDRESS_LEN],
        peer_chain_address: &[u8],
    ) {
        self.inner
            .note_peer(swap_id, peer_nim_address, peer_chain_address);
    }
    fn is_live(&self) -> bool {
        self.inner.is_live() // C1: a wrapper must never hide a live signer
    }
}

use crate::live_swap_signer::{
    AmoyHtlcSwapVerifier, EvmGasConfig, LiveInitiatorConfig, LiveInitiatorSigner, LivePollConfig,
    LiveResponderConfig, LiveResponderSigner, PeerBook, PolygonFundingStore,
};
use crate::nim_verifier::{NimFundingStore, NimHtlcVerifier};
use crate::nimiq::hex::hex_to_bytes;
use crate::nimiq::signer::InMemoryEnclaveKey;
use crate::polygon_gateway::HttpPolygonRpc;
use crate::rpc::HttpGatewayRpc;
use crate::swap::LadderParams;
use crate::swap_intent::{sign_intent_ephemeral, Asset, SwapIntent};
use crate::swap_leg::sha256;
use crate::swap_rate::RatePolicy;
use crate::swap_session::{NodeIdentity, SwapSession};
use crate::swap_usdc_leg::EvmAddress;
use crate::swap_wire::BTC_PUBKEY_LEN;

fn refused(reason: impl std::fmt::Display) -> LiveSwapFfiError {
    LiveSwapFfiError::Refused {
        reason: reason.to_string(),
    }
}

fn seed32(bytes: &[u8], what: &str) -> Result<[u8; 32], LiveSwapFfiError> {
    bytes
        .try_into()
        .map_err(|_| bad(format!("{what} must be 32 bytes")))
}

fn evm_addr(s: &str, what: &str) -> Result<EvmAddress, LiveSwapFfiError> {
    hex_to_bytes(s.trim())
        .ok()
        .and_then(|b| EvmAddress::try_from(b.as_slice()).ok())
        .ok_or_else(|| bad(format!("{what} must be a 20-byte 0x-hex address")))
}

fn ladder(delta: u64, window: u64) -> LadderParams {
    let d = LadderParams::default();
    LadderParams {
        delta_safe_blocks: if delta == 0 {
            d.delta_safe_blocks
        } else {
            delta
        },
        min_claim_window_blocks: if window == 0 {
            d.min_claim_window_blocks
        } else {
            window
        },
    }
}

/// A deterministic 33-byte BTC-pubkey FILLER for the USDC pair (the field is unused on
/// this pair but rides the wire and salts `derive_swap_id`, so it must be fresh per
/// advert and distinct per party — derived from the advert seed, never a real key).
fn btc_pubkey_filler(seed: &[u8; 32]) -> [u8; BTC_PUBKEY_LEN] {
    let mut buf = seed.to_vec();
    buf.extend_from_slice(b"nimmesh-live-btc-pubkey-filler-v1");
    let h = sha256(&buf);
    let mut k = [0u8; BTC_PUBKEY_LEN];
    k[0] = 0x02;
    k[1..].copy_from_slice(&h);
    k
}

/// The G11 per-swap secret PRF over a caller-CSPRNG seed (the exact
/// `new_swap_participant` recipe): unpredictable without the seed, distinct per swap,
/// domain-separated from the seed's identity use; no secret ever crosses back over FFI.
fn secret_source(seed: &[u8; 32]) -> crate::swap_session::SecretSource {
    let master = {
        let mut buf = seed.to_vec();
        buf.extend_from_slice(b"nimmesh-swap-secret-master-v1");
        sha256(&buf)
    };
    Box::new(move |swap_id| {
        let mut buf = master.to_vec();
        buf.extend_from_slice(swap_id);
        buf.extend_from_slice(b"nimmesh-swap-secret-v1");
        sha256(&buf)
    })
}

/// Build + ephemerally sign the live NIM⇄USDC standing intent. Pure — unit-testable.
pub(crate) fn build_live_intent(
    gives: Asset,
    nim_luna: u64,
    usdc_micro: u64,
    expiry_height: u64,
    seed: &[u8; 32],
    evm_address: EvmAddress,
) -> Result<SwapIntent, LiveSwapFfiError> {
    if nim_luna == 0 || usdc_micro == 0 {
        return Err(bad("trade amounts must be non-zero"));
    }
    if expiry_height == 0 {
        return Err(bad("expiry_height must be non-zero"));
    }
    if !matches!(gives, Asset::Nim | Asset::Usdc) {
        return Err(bad("live pair is NIM⇄USDC only"));
    }
    let mut intent = SwapIntent {
        gives,
        counter_asset: Asset::Usdc,
        nim_amount: nim_luna,
        btc_amount: usdc_micro,
        expiry_height,
        min_nim: 0,
        max_nim: u64::MAX,
        nim_pubkey: [0; 32],
        nim_address: [0; NIM_ADDRESS_LEN],
        btc_pubkey: btc_pubkey_filler(seed),
        // The chain-agnostic payout bytes the protocol carries: for the USDC pair this
        // IS the party's 20-byte EVM address (claim addr for a NIM-giver, funding/refund
        // addr for the USDC-giver) — the A2c wiring, byte for byte.
        btc_address: evm_address.to_vec(),
        evm_address,
        network_id: crate::NetworkId::Testnet.wire_id(),
        signature: [0; 64],
    };
    sign_intent_ephemeral(&mut intent, seed);
    debug_assert!(intent.verify_authentic());
    Ok(intent)
}

pub(super) fn build_initiator(
    sender_id: Vec<u8>,
    radio: Arc<dyn BleRadio>,
    nim_funding_key: Arc<dyn EnclaveKey>,
    lock_book: Arc<LiveLockBook>,
    config: FfiLiveInitiatorConfig,
    gateway_rpc_url: Option<String>,
) -> Result<Arc<MeshNode>, LiveSwapFfiError> {
    let seed = seed32(&config.intent_seed, "intent_seed")?;
    let gas_secret = seed32(&config.evm_gas_secret, "evm_gas_secret")?;
    let claim_addr: EvmAddress = config
        .evm_claim_address
        .as_slice()
        .try_into()
        .map_err(|_| bad("evm_claim_address must be 20 bytes"))?;
    let evm_gas_key = crate::evm_signer::LocalEvmKey::from_secret(&gas_secret)
        .map_err(|_| bad("evm_gas_secret is not a valid secp256k1 scalar"))?;
    let htlc = evm_addr(&config.htlc_address, "htlc_address")?;

    // Both chain clients are guard-pinned: testnet NIM, Amoy Polygon. No mainnet path.
    let nim_rpc: Arc<dyn GatewayRpc> = Arc::new(
        HttpGatewayRpc::new(&config.nim_rpc_url, crate::NetworkId::Testnet).map_err(refused)?,
    );
    let amoy: Arc<dyn crate::live_swap_signer::AmoyChain> = Arc::new(
        HttpPolygonRpc::new(config.amoy_rpc_url.clone()).map_err(|e| refused(e.to_string()))?,
    );

    let intent = build_live_intent(
        Asset::Nim,
        config.nim_luna,
        config.usdc_micro,
        config.expiry_height,
        &seed,
        claim_addr,
    )?;

    // The ephemeral key IS this node's swap identity (G45): intents verify against it
    // and every Propose is signed under it. The WALLET key never appears here — it only
    // signs the NIM HTLC funding inside the live signer.
    let propose_key = Arc::new(InMemoryEnclaveKey::from_secret(&seed));
    let identity = NodeIdentity {
        nim_address: intent.nim_address,
        btc_address: claim_addr.to_vec(),
        btc_pubkey: intent.btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: 1, // live: one swap in flight per construction
        standing_intent: Some(intent),
    };

    let peer_book = Arc::new(PeerBook::new());
    let polygon_store = Arc::new(PolygonFundingStore::new());
    let session = SwapSession::new(
        identity,
        ladder(config.delta_safe_blocks, config.min_claim_window_blocks),
    )
    .with_propose_signer(propose_key)
    .with_secret_source(secret_source(&seed))
    .with_funding_verifier(Box::new(AmoyHtlcSwapVerifier::new(
        amoy.clone(),
        htlc,
        claim_addr,
        polygon_store.clone(),
    )))
    .with_counterparty_chain(Asset::Usdc);
    // C1: surface the money-path gate as an Err at the door (MeshNode::build re-asserts
    // it as the last-resort invariant — this ctor must never rely on remembering it).
    if let Err(reason) = session.live_safety() {
        return Err(bad(format!("live-safety refused: {reason}")));
    }

    let signer = LiveInitiatorSigner::new(LiveInitiatorConfig {
        nim_key: nim_funding_key,
        nim_rpc,
        evm_gas_key,
        amoy,
        htlc,
        store: polygon_store,
        peer_book,
        gas: EvmGasConfig::default(),
        poll: LivePollConfig::default(),
    });
    // Latch OUTSIDE the recorder: a latched-away repeat never reaches the book.
    let signer = OneShotSigner::new(LockRecordingSigner::new(signer, lock_book));

    let gateway = match gateway_rpc_url {
        None => None,
        Some(url) => Some(
            crate::swap_participant_ffi::build_testnet_gateway(url)
                .map_err(|e| refused(e.to_string()))?,
        ),
    };
    Ok(MeshNode::build(
        sender_id,
        radio,
        gateway,
        crate::relay::RelayPolicy::production(),
        true,
        Some(session),
        Some(Box::new(signer)),
        crate::NetworkId::Testnet,
    ))
}

pub(super) fn build_responder(
    sender_id: Vec<u8>,
    radio: Arc<dyn BleRadio>,
    config: FfiLiveResponderConfig,
    gateway_rpc_url: Option<String>,
) -> Result<Arc<MeshNode>, LiveSwapFfiError> {
    let claim_seed = seed32(&config.nim_claim_seed, "nim_claim_seed")?;
    let evm_secret = seed32(&config.evm_funding_secret, "evm_funding_secret")?;
    let evm_key = crate::evm_signer::LocalEvmKey::from_secret(&evm_secret)
        .map_err(|_| bad("evm_funding_secret is not a valid secp256k1 scalar"))?;
    let evm_funded = evm_key.address();
    let htlc = evm_addr(&config.htlc_address, "htlc_address")?;
    let usdc = evm_addr(&config.usdc_address, "usdc_address")?;

    let nim_rpc: Arc<dyn GatewayRpc> = Arc::new(
        HttpGatewayRpc::new(&config.nim_rpc_url, crate::NetworkId::Testnet).map_err(refused)?,
    );
    let amoy: Arc<dyn crate::live_swap_signer::AmoyChain> = Arc::new(
        HttpPolygonRpc::new(config.amoy_rpc_url.clone()).map_err(|e| refused(e.to_string()))?,
    );

    // The responder's session identity IS its NIM claim key (the A2c wiring): the intent
    // signs under it, the Accept carries its address, and the live signer's claim key
    // must own it — enforced again inside `claim_nim`.
    let intent = build_live_intent(
        Asset::Usdc,
        config.nim_luna,
        config.usdc_micro,
        config.expiry_height,
        &claim_seed,
        evm_funded,
    )?;
    let nim_claim_key = Arc::new(InMemoryEnclaveKey::from_secret(&claim_seed));
    let identity = NodeIdentity {
        nim_address: intent.nim_address,
        btc_address: evm_funded.to_vec(),
        btc_pubkey: intent.btc_pubkey,
        rate_policy: RatePolicy::accept_all(),
        max_concurrent_swaps: 1, // live: one swap in flight per construction
        standing_intent: Some(intent),
    };

    let peer_book = Arc::new(PeerBook::new());
    let nim_store = Arc::new(NimFundingStore::new());
    let session = SwapSession::new(
        identity,
        ladder(config.delta_safe_blocks, config.min_claim_window_blocks),
    )
    // The responder never draws S, but the C1 gate (rightly) demands a non-sim source
    // on every live session — derive one from the claim seed's domain-separated PRF.
    .with_secret_source(secret_source(&claim_seed))
    .with_funding_verifier(Box::new(NimHtlcVerifier::new(
        nim_rpc.clone(),
        nim_store.clone(),
    )))
    .with_counterparty_chain(Asset::Usdc);
    // C1: surface the money-path gate as an Err at the door.
    if let Err(reason) = session.live_safety() {
        return Err(bad(format!("live-safety refused: {reason}")));
    }

    let signer = LiveResponderSigner::new(LiveResponderConfig {
        evm_key,
        amoy,
        htlc,
        usdc,
        peer_book,
        nim_claim_key,
        nim_rpc,
        nim_store,
        gas: EvmGasConfig::default(),
        poll: LivePollConfig::default(),
    });
    let signer = OneShotSigner::new(signer);

    let gateway = match gateway_rpc_url {
        None => None,
        Some(url) => Some(
            crate::swap_participant_ffi::build_testnet_gateway(url)
                .map_err(|e| refused(e.to_string()))?,
        ),
    };
    Ok(MeshNode::build(
        sender_id,
        radio,
        gateway,
        crate::relay::RelayPolicy::production(),
        true,
        Some(session),
        Some(Box::new(signer)),
        crate::NetworkId::Testnet,
    ))
}
