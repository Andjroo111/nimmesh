//! # swap_participant_ffi — the production swap-participant constructor (G9 slice 3 / OG-6)
//!
//! Until now every swap **participant** node — one that runs a [`crate::swap_session::SwapSession`],
//! advertises a standing [`crate::swap_intent::SwapIntent`], matches counterparties, and drives the
//! `Propose → Accept → Fund → Reveal → Settle` protocol over the mesh — was a `#[cfg(test)]`
//! constructor. This module is the production door: a third `#[uniffi::export]` block on
//! [`MeshNode`] (the `gateway_ffi` pattern) that lets the iOS app and the Mac node build a real
//! participant over the real BLE mesh.
//!
//! **Sim money only (the `MockSigner` seam):** the swap *protocol* runs for real over the radio —
//! discovery, matching, signed Proposes, funding proofs, preimage reveal, retransmit, refund
//! safety, GC — but the on-chain tx bytes are still the deterministic sim stand-ins. The real
//! NIM/BTC signer drops into the same [`crate::swap_signer::SwapSigner`] seam later (money-path,
//! gated); nothing constructed here can move funds.
//!
//! **Testnet-pinned by construction:** like [`crate::gateway_ffi`]'s testnet gateway, there is no
//! mainnet parameter here. The node, its intent's `network_id`, and the optional gateway hop are
//! all Albatross testnet. Lifting that is a deliberate, Andjroo-gated change — not a parameter.
//!
//! **Privacy (G45):** the intent + Propose identity is an **ephemeral** Ed25519 key built from
//! caller-supplied per-advertisement randomness (`intent_seed`) — NOT the wallet key, which never
//! crosses this boundary. The seed enters once, lives inside the session's enclave-seam objects,
//! and two advertisements with fresh seeds don't link on the NIM identity
//! (`docs/swap/DISCOVERY-PRIVACY.md`).

use std::sync::Arc;

use crate::node::MeshNode;
use crate::radio::BleRadio;
use crate::swap::LadderParams;
use crate::swap_intent::{sign_intent_ephemeral, Asset, SwapIntent, EVM_ADDRESS_LEN};
use crate::swap_rate::RatePolicy;
use crate::swap_session::{NodeIdentity, SwapSession, DEFAULT_MAX_CONCURRENT_SWAPS};
use crate::swap_wire::{BTC_PUBKEY_LEN, NIM_ADDRESS_LEN};

/// Which asset a standing intent **gives** (funds) in the trade it advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiIntentAsset {
    /// Give NIM (the advertiser would be the swap initiator).
    Nim,
    /// Give BTC (the advertiser would be the responder).
    Btc,
    /// Give USDC on Polygon (the advertiser would be the responder).
    Usdc,
}

impl From<FfiIntentAsset> for Asset {
    fn from(a: FfiIntentAsset) -> Self {
        match a {
            FfiIntentAsset::Nim => Asset::Nim,
            FfiIntentAsset::Btc => Asset::Btc,
            FfiIntentAsset::Usdc => Asset::Usdc,
        }
    }
}

/// The trade this node advertises on the mesh (its standing discovery intent, G34).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiStandingIntent {
    /// Which asset this node funds.
    pub gives: FfiIntentAsset,
    /// The non-NIM asset of the trade (a NIM giver names what it wants; a BTC/USDC giver
    /// names its own asset).
    pub counter_asset: FfiIntentAsset,
    /// NIM in the trade, in luna.
    pub nim_amount: u64,
    /// The counter asset in the trade — satoshis for BTC, micro-USDC for USDC.
    pub counter_amount: u64,
    /// Absolute chain height after which this advertisement expires (G35). Anchor to the
    /// best-known head plus a freshness window.
    pub expiry_height: u64,
    /// Smallest acceptable NIM trade size in luna (G40); `0` = no floor.
    pub min_nim: u64,
    /// Largest acceptable NIM trade size in luna (G40); `u64::MAX` = no ceiling.
    pub max_nim: u64,
}

/// Everything a production swap participant needs at construction.
// `Debug` is hand-written in `crate::ffi_secret_redaction` — it MUST NOT be derived here: the
// derive renders `intent_seed` as raw bytes at any `{:?}`.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiParticipantConfig {
    /// This node's BTC claimant/funder pubkey (33 bytes, compressed) carried in its intent.
    pub btc_pubkey: Vec<u8>,
    /// This node's BTC payout address bytes carried in its intent.
    pub btc_address: Vec<u8>,
    /// Max concurrent swaps this node will hold (anti-DoS); `0` = the default (16).
    pub max_concurrent_swaps: u32,
    /// Ladder: minimum required `T_A − T_B` gap, in blocks (Δ_safe).
    pub delta_safe_blocks: u64,
    /// Ladder: minimum required `T_B − head` window before funding, in blocks.
    pub min_claim_window_blocks: u64,
    /// The trade to advertise; `None` = participate without advertising (respond only).
    pub standing_intent: Option<FfiStandingIntent>,
    /// 32 bytes of fresh randomness for the **ephemeral** intent/Propose identity (G45).
    /// Draw from the platform CSPRNG per advertisement; never pass a wallet seed.
    pub intent_seed: Vec<u8>,
}

/// A failure constructing a swap participant across FFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum ParticipantInitError {
    /// A config field was malformed (wrong-length seed/pubkey, contradictory assets…).
    BadInput {
        /// A human-readable reason.
        reason: String,
    },
    /// The optional gateway hop was requested but refused (bad URL / mainnet host), or
    /// this build has no HTTP client (`gateway-rpc` feature off).
    Gateway {
        /// A human-readable reason.
        reason: String,
    },
}

impl std::fmt::Display for ParticipantInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParticipantInitError::BadInput { reason } => write!(f, "bad input: {reason}"),
            ParticipantInitError::Gateway { reason } => write!(f, "gateway refused: {reason}"),
        }
    }
}

impl std::error::Error for ParticipantInitError {}

fn bad(reason: impl Into<String>) -> ParticipantInitError {
    ParticipantInitError::BadInput {
        reason: reason.into(),
    }
}

/// Build + ephemerally sign the standing [`SwapIntent`] from the FFI config (G41/G45). Pure —
/// unit-testable without a node. Returns the signed intent (its `nim_address`/`nim_pubkey` are the
/// ephemeral identity derived from `seed`).
pub(crate) fn build_signed_intent(
    cfg: &FfiStandingIntent,
    btc_pubkey: [u8; BTC_PUBKEY_LEN],
    btc_address: Vec<u8>,
    seed: &[u8; 32],
) -> Result<SwapIntent, ParticipantInitError> {
    let gives: Asset = cfg.gives.into();
    let counter: Asset = cfg.counter_asset.into();
    // The counter asset is the trade's non-NIM asset: a NIM giver must want a non-NIM asset,
    // and a non-NIM giver's counter asset is itself (see `SwapIntent::counter_asset`).
    let coherent = match gives {
        Asset::Nim => counter != Asset::Nim,
        Asset::Btc | Asset::Usdc => counter == gives,
    };
    if !coherent {
        return Err(bad("gives/counter_asset are contradictory"));
    }
    if cfg.nim_amount == 0 || cfg.counter_amount == 0 {
        return Err(bad("intent amounts must be non-zero"));
    }
    if cfg.min_nim > cfg.max_nim {
        return Err(bad("min_nim exceeds max_nim"));
    }
    let mut intent = SwapIntent {
        gives,
        counter_asset: counter,
        nim_amount: cfg.nim_amount,
        btc_amount: cfg.counter_amount,
        expiry_height: cfg.expiry_height,
        min_nim: cfg.min_nim,
        max_nim: cfg.max_nim,
        nim_pubkey: [0; 32],
        nim_address: [0; NIM_ADDRESS_LEN],
        btc_pubkey,
        btc_address,
        // No EVM identity over this constructor yet — NIM⇄BTC first (the USDC leg rides the
        // same discovery once an EVM address field is plumbed through the config).
        evm_address: [0; EVM_ADDRESS_LEN],
        network_id: crate::NetworkId::Testnet.wire_id(),
        signature: [0; 64],
    };
    sign_intent_ephemeral(&mut intent, seed);
    debug_assert!(intent.verify_authentic());
    Ok(intent)
}

#[uniffi::export]
impl MeshNode {
    /// Create a **swap participant** node on the Albatross TESTNET: a full mesh node that also
    /// runs the swap session — it advertises `config.standing_intent` (if any) on the bounded
    /// re-advertise schedule, matches complementary intents, and drives the swap protocol over
    /// the mesh with **sim tx bytes** (`MockSigner`; no funds can move). Watch progress with
    /// [`MeshNode::active_swaps`] + [`MeshNode::discovery_metrics`].
    ///
    /// `gateway_rpc_url` optionally makes it ALSO a testnet gateway (the Mac responder rig —
    /// beacons the head the ladder/expiry logic anchors to). Requires the `gateway-rpc` feature;
    /// a phone participant passes `None` and hears beacons from the mesh instead.
    #[uniffi::constructor]
    pub fn new_swap_participant(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        config: FfiParticipantConfig,
        gateway_rpc_url: Option<String>,
    ) -> Result<Arc<Self>, ParticipantInitError> {
        let seed: [u8; 32] = config
            .intent_seed
            .as_slice()
            .try_into()
            .map_err(|_| bad("intent_seed must be 32 bytes"))?;
        let btc_pubkey: [u8; BTC_PUBKEY_LEN] = config
            .btc_pubkey
            .as_slice()
            .try_into()
            .map_err(|_| bad("btc_pubkey must be 33 bytes"))?;
        if config.btc_address.is_empty() || config.btc_address.len() > 64 {
            return Err(bad("btc_address must be 1..=64 bytes"));
        }
        if config.min_claim_window_blocks == 0 || config.delta_safe_blocks == 0 {
            return Err(bad("ladder blocks must be non-zero"));
        }

        let standing = config
            .standing_intent
            .as_ref()
            .map(|c| build_signed_intent(c, btc_pubkey, config.btc_address.clone(), &seed))
            .transpose()?;

        // The ephemeral key IS this node's swap identity: intents verify against it (G41) and
        // every Propose this node originates is signed under it (S2/#73) — `recv_propose`
        // enforces that the proposer's pubkey hashes to the proposal's `nim_address`.
        let propose_key = crate::nimiq::signer::InMemoryEnclaveKey::from_secret(&seed);
        let nim_address = *crate::nimiq::address::Address::from_public_key(
            &crate::nimiq::signer::EnclaveKey::public_key(&propose_key)
                .try_into()
                .expect("ed25519 pubkey is 32 bytes"),
        )
        .as_bytes();

        let identity = NodeIdentity {
            nim_address,
            btc_address: config.btc_address.clone(),
            btc_pubkey,
            rate_policy: RatePolicy::accept_all(),
            max_concurrent_swaps: match config.max_concurrent_swaps {
                0 => DEFAULT_MAX_CONCURRENT_SWAPS,
                n => n as usize,
            },
            standing_intent: standing,
        };
        let ladder = LadderParams {
            delta_safe_blocks: config.delta_safe_blocks,
            min_claim_window_blocks: config.min_claim_window_blocks,
        };
        // G11: replace the deterministic sim secret with a CSPRNG-backed PRF — the per-swap
        // secret is sha256(master ‖ swap_id ‖ label), master = sha256(caller's CSPRNG seed ‖
        // label). Unpredictable without the seed, distinct per swap, domain-separated from
        // the seed's identity use. No secret ever crosses back over FFI.
        let secret_master = {
            let mut buf = seed.to_vec();
            buf.extend_from_slice(b"nimmesh-swap-secret-master-v1");
            crate::swap_leg::sha256(&buf)
        };
        let session = SwapSession::new(identity, ladder)
            .with_propose_signer(Arc::new(propose_key))
            .with_secret_source(Box::new(move |swap_id| {
                let mut buf = secret_master.to_vec();
                buf.extend_from_slice(swap_id);
                buf.extend_from_slice(b"nimmesh-swap-secret-v1");
                crate::swap_leg::sha256(&buf)
            }));

        let gateway = match gateway_rpc_url {
            None => None,
            Some(url) => Some(build_testnet_gateway(url)?),
        };

        // Verify-before-relay ON (G12 spam filter) + production relay policy, same as the
        // shipping `new`/`new_gateway` — this is a production node, not a harness one.
        Ok(Self::build(
            sender_id,
            radio,
            gateway,
            crate::relay::RelayPolicy::production(),
            true,
            Some(session),
            Some(Box::new(crate::swap_signer::MockSigner)),
            crate::NetworkId::Testnet,
        ))
    }
}

// --- the rig door (NOT on the FFI surface) --------------------------------------------------

impl MeshNode {
    /// A2b: the **rig/e2e** participant constructor — build a swap participant from a
    /// caller-COMPOSED [`SwapSession`] (its funding verifier, confirmation policy,
    /// counterparty chain, secret source, and propose signer already injected via the
    /// session's builders) plus a caller-supplied [`SwapSigner`] — the door the live
    /// NIM⇄USDC signer walks in through ([`crate::live_swap_signer`]). Deliberately NOT
    /// `#[uniffi::export]`: it takes raw trait objects (a phone builds through
    /// [`MeshNode::new_swap_participant`] instead), and it is TESTNET-pinned by
    /// construction like every non-gated door. Deterministic relay policy — this is the
    /// harness/rig path, never the shipping phone node.
    pub fn new_session_participant(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        session: SwapSession,
        signer: Box<dyn crate::swap_signer::SwapSigner>,
    ) -> Arc<Self> {
        Self::build(
            sender_id,
            radio,
            None,
            crate::relay::RelayPolicy::deterministic(),
            true,
            Some(session),
            Some(signer),
            crate::NetworkId::Testnet,
        )
    }
}

/// The optional testnet gateway hop (`gateway-rpc` builds only; others refuse honestly so the
/// shared bindings never diverge — the `gateway_ffi` discipline). `pub(crate)`: the live
/// swap constructors (`swap_live_ffi`) reuse the same guarded hop.
#[cfg(feature = "gateway-rpc")]
pub(crate) fn build_testnet_gateway(
    url: String,
) -> Result<Arc<dyn crate::gateway::MeshGateway>, ParticipantInitError> {
    let rpc = crate::rpc::HttpGatewayRpc::new(url, crate::NetworkId::Testnet).map_err(|e| {
        ParticipantInitError::Gateway {
            reason: e.to_string(),
        }
    })?;
    Ok(Arc::new(crate::gateway::RpcGateway::new(Arc::new(rpc))))
}

#[cfg(not(feature = "gateway-rpc"))]
pub(crate) fn build_testnet_gateway(
    _url: String,
) -> Result<Arc<dyn crate::gateway::MeshGateway>, ParticipantInitError> {
    Err(ParticipantInitError::Gateway {
        reason: "core built without the gateway-rpc feature".into(),
    })
}

/// **OWNER-GATED (real money): the MAINNET twin of [`build_testnet_gateway`].** The optional
/// mainnet gateway hop for a mainnet live-swap participant (the Mac rig), built via
/// [`crate::rpc::HttpGatewayRpc::new_mainnet`] + [`crate::gateway::RpcGateway::new_mainnet`]
/// (network id `24`). Reachable ONLY from the off-by-default mainnet swap assembly
/// (`swap_live_ffi::live_impl`, which requires `gateway-rpc`); the autonomous/testnet path never
/// calls it. Only the `gateway-rpc` build defines it — the not-feature door refuses before it here.
#[cfg(feature = "gateway-rpc")]
pub(crate) fn build_mainnet_gateway(
    url: String,
) -> Result<Arc<dyn crate::gateway::MeshGateway>, ParticipantInitError> {
    let rpc = crate::rpc::HttpGatewayRpc::new_mainnet(url).map_err(|e| {
        ParticipantInitError::Gateway {
            reason: e.to_string(),
        }
    })?;
    Ok(Arc::new(crate::gateway::RpcGateway::new_mainnet(Arc::new(
        rpc,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_radio::{MockEther, MockRadio};

    fn radio(id: &str) -> Arc<dyn BleRadio> {
        MockRadio::new(id, MockEther::new())
    }

    fn intent_cfg(gives: FfiIntentAsset, counter: FfiIntentAsset) -> FfiStandingIntent {
        FfiStandingIntent {
            gives,
            counter_asset: counter,
            nim_amount: 200_000,
            counter_amount: 100_000,
            expiry_height: 1_000_000,
            min_nim: 0,
            max_nim: u64::MAX,
        }
    }

    fn cfg(standing: Option<FfiStandingIntent>) -> FfiParticipantConfig {
        FfiParticipantConfig {
            btc_pubkey: vec![0x02; BTC_PUBKEY_LEN],
            btc_address: b"tb1qexample".to_vec(),
            max_concurrent_swaps: 0,
            delta_safe_blocks: 600,
            min_claim_window_blocks: 600,
            standing_intent: standing,
            intent_seed: vec![0x5A; 32],
        }
    }

    #[test]
    fn signed_intent_is_authentic_and_maps_the_config() {
        let c = intent_cfg(FfiIntentAsset::Nim, FfiIntentAsset::Btc);
        let intent = build_signed_intent(
            &c,
            [0x02; BTC_PUBKEY_LEN],
            b"tb1qexample".to_vec(),
            &[7; 32],
        )
        .expect("valid config signs");
        assert!(intent.verify_authentic());
        assert_eq!(intent.gives, Asset::Nim);
        assert_eq!(intent.counter_asset, Asset::Btc);
        assert_eq!(intent.nim_amount, 200_000);
        assert_eq!(intent.btc_amount, 100_000);
        assert_eq!(intent.network_id, crate::NetworkId::Testnet.wire_id());
        // The identity is the ephemeral key's, not zeroes.
        assert_ne!(intent.nim_address, [0; NIM_ADDRESS_LEN]);
    }

    #[test]
    fn contradictory_assets_and_degenerate_amounts_are_refused() {
        // A NIM giver must want a non-NIM counter asset.
        let c = intent_cfg(FfiIntentAsset::Nim, FfiIntentAsset::Nim);
        assert!(build_signed_intent(&c, [2; 33], vec![1], &[7; 32]).is_err());
        // A BTC giver's counter asset is itself, not USDC.
        let c = intent_cfg(FfiIntentAsset::Btc, FfiIntentAsset::Usdc);
        assert!(build_signed_intent(&c, [2; 33], vec![1], &[7; 32]).is_err());
        // Zero amounts never advertise.
        let mut c = intent_cfg(FfiIntentAsset::Nim, FfiIntentAsset::Btc);
        c.nim_amount = 0;
        assert!(build_signed_intent(&c, [2; 33], vec![1], &[7; 32]).is_err());
    }

    #[test]
    fn constructor_validates_seed_pubkey_and_ladder() {
        let mut c = cfg(None);
        c.intent_seed = vec![1; 16]; // not 32
        assert!(MeshNode::new_swap_participant(b"p".to_vec(), radio("a"), c, None).is_err());

        let mut c = cfg(None);
        c.btc_pubkey = vec![2; 20]; // not 33
        assert!(MeshNode::new_swap_participant(b"p".to_vec(), radio("b"), c, None).is_err());

        let mut c = cfg(None);
        c.min_claim_window_blocks = 0;
        assert!(MeshNode::new_swap_participant(b"p".to_vec(), radio("c"), c, None).is_err());
    }

    #[test]
    fn participant_constructs_with_a_standing_intent() {
        // The production ctor wires a live session (construct + observe + tear down only —
        // no ticking: it carries the production jitter policy, which must not run hot in
        // the parallel suite; the tick behavior is proven deterministically below).
        let node = MeshNode::new_swap_participant(
            b"ph".to_vec(),
            radio("d"),
            cfg(Some(intent_cfg(FfiIntentAsset::Nim, FfiIntentAsset::Btc))),
            None,
        )
        .expect("valid participant constructs");
        assert!(node.active_swaps().is_empty());
        node.shutdown();
    }

    #[test]
    fn standing_intent_advertises_on_the_beacon_tick() {
        // The on-device wiring proof: a participant re-advertises on the beacon heartbeat
        // (the shims never call poll_sync — gc_tick must ride BeaconTick or discovery is
        // dead on a real phone). Deterministic relay policy, same as every harness test —
        // the FFI-built signed intent rides a test-ctor node so no jitter threads run.
        let c = intent_cfg(FfiIntentAsset::Nim, FfiIntentAsset::Btc);
        let intent = build_signed_intent(
            &c,
            [0x02; BTC_PUBKEY_LEN],
            b"tb1qexample".to_vec(),
            &[7; 32],
        )
        .expect("valid config signs");
        let identity = NodeIdentity {
            nim_address: intent.nim_address,
            btc_address: intent.btc_address.clone(),
            btc_pubkey: intent.btc_pubkey,
            rate_policy: RatePolicy::accept_all(),
            max_concurrent_swaps: DEFAULT_MAX_CONCURRENT_SWAPS,
            standing_intent: Some(intent),
        };
        let ladder = LadderParams {
            delta_safe_blocks: 600,
            min_claim_window_blocks: 600,
        };
        let node = MeshNode::new_participant(
            b"ph".to_vec(),
            radio("d"),
            crate::relay::RelayPolicy::deterministic(),
            identity,
            ladder,
        );
        for _ in 0..3 {
            node.poll_beacon();
        }
        node.fence();
        assert!(
            node.discovery_metrics().readvertised >= 1,
            "standing intent must flood on the beacon tick"
        );
        node.shutdown();
    }

    #[test]
    fn gateway_url_participant_matches_build_features() {
        let c = cfg(None);
        let got = MeshNode::new_swap_participant(
            b"gw".to_vec(),
            radio("e"),
            c,
            Some("https://rpc.testnet.nimiqwatch.com".into()),
        );
        #[cfg(feature = "gateway-rpc")]
        {
            let node = got.expect("testnet gateway participant constructs");
            node.shutdown();
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            assert!(matches!(got, Err(ParticipantInitError::Gateway { .. })));
        }
    }
}
