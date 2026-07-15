//! # swap_live_ffi — the LIVE swap-participant constructors over UniFFI (G10a)
//!
//! Act 2 proved the real NIM⇄USDC money path headlessly (`live_swap_signer`, receipts in
//! `docs/swap/ACT2-RECEIPTS.md`) through the **rig door** (`new_session_participant`), which
//! deliberately never crossed FFI. This module is the production door: two more
//! `#[uniffi::export]` constructors on [`MeshNode`] (the `gateway_ffi` discipline — always
//! exported, honest refusal when the build lacks the HTTP stacks) that build a mesh swap
//! participant carrying the **live signer + the real funding verifier** for its role:
//!
//! - [`MeshNode::new_live_swap_initiator`] — the phone. NIM-giver: advertises "gives NIM,
//!   wants USDC", funds the REAL testnet NIM HTLC with the **wallet's enclave key** (the
//!   [`EnclaveKey`] foreign trait — the seed never crosses; only signed bytes do), gates its
//!   reveal on the real Amoy escrow via `AmoyHtlcSwapVerifier`, and lands `withdraw(S)` with
//!   a caller-supplied Amoy **gas** key (ADR-0011). The claimed USDC pays out to a plain
//!   receive address — no EVM custody needed to receive.
//! - [`MeshNode::new_live_swap_responder`] — the Mac rig. USDC-giver: escrows real Amoy USDC
//!   (gated by the real `NimHtlcVerifier` — it funds NOTHING until the NIM HTLC is on-chain
//!   at depth) and claims the NIM leg with the revealed `S`.
//!
//! **TESTNET/Amoy pinned by construction:** every chain client rides `rpc::guard_testnet` /
//! `polygon_gateway::guard_amoy`; there is no network parameter. Mainnet is a separate,
//! Andjroo-gated step — not a flag here.
//!
//! **One real funding per construction (the A2c latch, now in the door):** standing intents
//! re-advertise by design (G37), and #189's tombstone already refuses re-initiating a used
//! `swap_id` — but a *different* counterparty could still match a second time. A live
//! participant therefore carries a one-shot funding latch: after its first successful
//! on-chain funding it refuses every further funding (claims pass through), so one
//! construction can never move more than the one advertised trade. Start another swap by
//! constructing another participant (fresh seed → fresh identity → fresh `swap_id`).
//!
//! **Never strand (the refund seam):** every NIM HTLC the initiator actually funds is
//! recorded into the caller's [`LiveLockBook`] (decoded from the very wire that was
//! broadcast), and [`NimHtlcRefunder`] turns a recorded lock back into the wallet's funds
//! once its timeout passes — the phone-side twin of the A2c recovery pass.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nimiq::address::Address;
use crate::nimiq::hex::bytes_to_hex;
use crate::nimiq::htlc::{timeout_resolve_proof, HtlcRedeem};
use crate::nimiq::signer::EnclaveKey;
use crate::node::MeshNode;
use crate::radio::BleRadio;
use crate::rpc::GatewayRpc;

/// Bound on recorded locks (a participant funds once; a runaway caller is capped).
#[cfg_attr(
    not(all(feature = "polygon-gateway", feature = "gateway-rpc")),
    allow(dead_code)
)]
const MAX_LOCKS: usize = 32;
/// Post-timeout settle margin before a refund is attempted (chain clocks drift; the A2c
/// recovery used the same 60 s).
const REFUND_MARGIN_MS: u64 = 60_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A failure constructing or driving a live swap participant across FFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum LiveSwapFfiError {
    /// This build of the core has no live money-path stack (`polygon-gateway` +
    /// `gateway-rpc` features off) — the FFI surface stays identical, the door refuses.
    Unsupported,
    /// A config field was malformed (wrong-length seed/address, zero amounts…).
    BadInput {
        /// A human-readable reason.
        reason: String,
    },
    /// A chain client refused construction — a known mainnet host, or a malformed URL
    /// (`guard_testnet` / `guard_amoy`; the live participant is testnet/Amoy-only).
    Refused {
        /// A human-readable reason.
        reason: String,
    },
    /// A live RPC call failed (transient transport — retry).
    Transport {
        /// A human-readable reason.
        reason: String,
    },
}

impl std::fmt::Display for LiveSwapFfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveSwapFfiError::Unsupported => write!(
                f,
                "live swap unsupported: core built without polygon-gateway + gateway-rpc"
            ),
            LiveSwapFfiError::BadInput { reason } => write!(f, "bad input: {reason}"),
            LiveSwapFfiError::Refused { reason } => write!(f, "refused: {reason}"),
            LiveSwapFfiError::Transport { reason } => write!(f, "transport: {reason}"),
        }
    }
}

impl std::error::Error for LiveSwapFfiError {}

fn bad(reason: impl Into<String>) -> LiveSwapFfiError {
    LiveSwapFfiError::BadInput {
        reason: reason.into(),
    }
}

// --- the mainnet arming gate (pure; testable without the live features) ------------------------

/// The mainnet-arming predicate, factored so the mainnet swap assembly is unit-testable with an
/// injected `(allowed, htlc_address)` — WITHOUT ever flipping the shipped
/// [`crate::mainnet_swap::MAINNET_SWAP_ENABLED`] const. Returns the trimmed HTLC address iff the
/// master switch allows mainnet AND a deployed escrow contract is recorded; otherwise a clear
/// [`LiveSwapFfiError::Refused`]. The mainnet constructors call it with the shipped consts
/// (`live_swap_allowed(Mainnet)` = `false`, `MAINNET_HTLC_ADDRESS` = `""` on any merged branch), so
/// they **always refuse and the whole mainnet path is inert** until Andjroo's arming PR flips both.
pub(crate) fn mainnet_htlc_if_armed(
    allowed: bool,
    htlc_address: &str,
) -> Result<String, LiveSwapFfiError> {
    if !allowed {
        return Err(LiveSwapFfiError::Refused {
            reason: "mainnet swap is disabled (mainnet_swap::MAINNET_SWAP_ENABLED = false)".into(),
        });
    }
    let addr = htlc_address.trim();
    if addr.is_empty() {
        return Err(LiveSwapFfiError::Refused {
            reason: "mainnet HTLC address is not set (mainnet_swap::MAINNET_HTLC_ADDRESS is empty)"
                .into(),
        });
    }
    Ok(addr.to_string())
}

/// **App probe:** whether the mainnet swap path is fully armed (the master switch is on AND a
/// deployed HTLC address is recorded — see [`crate::mainnet_swap::mainnet_swap_armed`]). `false` on
/// any shipped/merged build, so the UI keeps its testnet path + labels; the arming release flips it.
#[uniffi::export]
pub fn mainnet_swap_armed() -> bool {
    crate::mainnet_swap::mainnet_swap_armed()
}

/// **App probe:** a human-readable reason for the current mainnet-swap arm state, so the UI can
/// label it honestly (e.g. "REAL MAINNET FUNDS" when armed, or why it is disabled when not).
#[uniffi::export]
pub fn mainnet_swap_reason() -> String {
    match mainnet_htlc_if_armed(
        crate::mainnet_swap::live_swap_allowed(crate::NetworkId::Mainnet),
        crate::mainnet_swap::MAINNET_HTLC_ADDRESS,
    ) {
        Ok(htlc) => format!("armed — real mainnet funds; escrow {htlc}"),
        Err(LiveSwapFfiError::Refused { reason }) => reason,
        Err(e) => e.to_string(),
    }
}

/// Everything the PHONE (NIM-giver / initiator) needs to run one real testnet⇄Amoy swap.
/// Secrets in this record enter the core once and never cross back (the G45/G11 rule); the
/// wallet's NIM key is NOT here — it stays behind the [`EnclaveKey`] foreign trait argument.
// `Debug` is hand-written in `crate::ffi_secret_redaction` — it MUST NOT be derived here: the
// derive renders `intent_seed`/`evm_gas_secret` as raw bytes at any `{:?}`.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiLiveInitiatorConfig {
    /// NIM this node gives, in luna.
    pub nim_luna: u64,
    /// USDC it wants in return, in micro-USDC (6 decimals).
    pub usdc_micro: u64,
    /// Absolute testnet height after which the advertisement expires (head + window).
    pub expiry_height: u64,
    /// 32 bytes of fresh platform-CSPRNG randomness: the ephemeral intent/Propose identity
    /// (never the wallet key) AND the master for the per-swap secret PRF (G11/G45).
    pub intent_seed: Vec<u8>,
    /// The 20-byte EVM address the claimed USDC pays out to (receive-only — no EVM key
    /// custody needed to receive; ADR-0007's caller-open withdraw).
    pub evm_claim_address: Vec<u8>,
    /// 32-byte secp256k1 secret for the Amoy account that pays the claim's GAS (needs a
    /// little POL; ADR-0011). Distinct from the payout address by design.
    pub evm_gas_secret: Vec<u8>,
    /// The Albatross TESTNET JSON-RPC url (`guard_testnet` enforced).
    pub nim_rpc_url: String,
    /// The Polygon AMOY JSON-RPC url (`guard_amoy` enforced).
    pub amoy_rpc_url: String,
    /// The deployed `NimmeshHtlc` v2 address, 0x-hex (20 bytes).
    pub htlc_address: String,
    /// Ladder: minimum required `T_A − T_B` gap, in blocks (Δ_safe); 0 = the default.
    pub delta_safe_blocks: u64,
    /// Ladder: minimum required `T_B − head` window before funding, in blocks; 0 = default.
    pub min_claim_window_blocks: u64,
}

/// Everything the MAC RIG (USDC-giver / responder) needs to answer one real swap.
/// (No `term_anchor` in either config: M4 wires the anchor into each swap's own
/// `SwapContext` from the head its terms were minted against.)
// `Debug` is hand-written in `crate::ffi_secret_redaction` — see the initiator config above.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiLiveResponderConfig {
    /// USDC this node gives, in micro-USDC.
    pub usdc_micro: u64,
    /// NIM it wants in return, in luna.
    pub nim_luna: u64,
    /// Absolute testnet height after which the advertisement expires.
    pub expiry_height: u64,
    /// 32-byte Ed25519 seed owning this node's NIM CLAIM address — its session identity,
    /// its intent signature, and the key that redeems the NIM HTLC. A rig seed (derive it
    /// from the documented treasury label so claimed funds stay recoverable), never a
    /// user wallet.
    pub nim_claim_seed: Vec<u8>,
    /// 32-byte secp256k1 secret of the FUNDED Amoy account (escrows the USDC + pays gas).
    pub evm_funding_secret: Vec<u8>,
    /// The Albatross TESTNET JSON-RPC url (`guard_testnet` enforced).
    pub nim_rpc_url: String,
    /// The Polygon AMOY JSON-RPC url (`guard_amoy` enforced).
    pub amoy_rpc_url: String,
    /// The deployed `NimmeshHtlc` v2 address, 0x-hex (20 bytes).
    pub htlc_address: String,
    /// The Amoy USDC token the HTLC escrows, 0x-hex (20 bytes).
    pub usdc_address: String,
    /// Ladder: minimum required `T_A − T_B` gap, in blocks; 0 = the default.
    pub delta_safe_blocks: u64,
    /// Ladder: minimum required `T_B − head` window before funding, in blocks; 0 = default.
    pub min_claim_window_blocks: u64,
}

/// One REAL NIM HTLC this device funded: everything the refund path needs, nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiNimLock {
    /// The HTLC contract address (user-friendly `NQ…`).
    pub contract: String,
    /// The locked luna.
    pub value: u64,
    /// The HTLC timeout (Unix **milliseconds**) — refundable once passed.
    pub timeout_ms: u64,
    /// The funding tx hash (lowercase hex) — the chain receipt to show/verify.
    pub funding_tx_hash: String,
}

/// The caller-held book of REAL NIM locks a live initiator funded. The app constructs one,
/// passes it into [`MeshNode::new_live_swap_initiator`], keeps the handle, and persists
/// [`LiveLockBook::locks`] — so a swap that dies mid-flight is always refundable later
/// (never-strand). Records are decoded from the exact broadcast wire; bounded; no secrets.
#[derive(Default, uniffi::Object)]
pub struct LiveLockBook {
    locks: Mutex<Vec<FfiNimLock>>,
}

#[uniffi::export]
impl LiveLockBook {
    /// An empty book.
    #[uniffi::constructor]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Arc<Self> {
        Arc::new(LiveLockBook::default())
    }

    /// Every NIM HTLC recorded so far (oldest first).
    pub fn locks(&self) -> Vec<FfiNimLock> {
        self.locks.lock().unwrap().clone()
    }
}

impl LiveLockBook {
    /// Record a funded lock (deduped by contract; bounded).
    #[cfg_attr(
        not(all(feature = "polygon-gateway", feature = "gateway-rpc")),
        allow(dead_code)
    )]
    pub(crate) fn record(&self, lock: FfiNimLock) {
        let mut locks = self.locks.lock().unwrap();
        if locks.len() >= MAX_LOCKS || locks.iter().any(|l| l.contract == lock.contract) {
            return;
        }
        locks.push(lock);
    }
}

/// The outcome of one refund attempt (idempotent — call again until `AlreadyResolved`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiRefundOutcome {
    /// The refund tx was broadcast — poll again; only `AlreadyResolved` proves it landed.
    Refunded {
        /// The refund tx hash (lowercase hex).
        tx_hash: String,
    },
    /// The contract is already empty/gone **on this refunder's chain** — claimed by the
    /// counterparty or refunded. A caller probing more than one chain (a lock record does
    /// not carry its network) may forget the lock ONLY when every probed chain reads
    /// `AlreadyResolved`; a wrong-chain read alone proves nothing.
    AlreadyResolved,
    /// The HTLC timeout has not passed yet — try again after `until_ms`.
    StillLocked {
        /// Unix ms after which the refund becomes possible.
        until_ms: u64,
    },
}

/// The phone-side refund tool (never-strand): turns a recorded [`FfiNimLock`] back into the
/// funding wallet's balance once its timeout passed, via the NIM HTLC's `TimeoutResolve`
/// path — signed by the SAME wallet enclave key that funded it (the HTLC's `htlc_sender`).
/// Testnet-pinned like every live door.
#[derive(uniffi::Object)]
pub struct NimHtlcRefunder {
    key: Arc<dyn EnclaveKey>,
    rpc: Arc<dyn GatewayRpc>,
    network: crate::NetworkId,
    clock_ms: fn() -> u64,
}

#[uniffi::export]
impl NimHtlcRefunder {
    /// A refunder over the wallet's enclave key and a TESTNET RPC url. Requires the
    /// `gateway-rpc` feature; refuses honestly otherwise (shared-bindings discipline).
    #[uniffi::constructor]
    pub fn new(
        nim_key: Arc<dyn EnclaveKey>,
        nim_rpc_url: String,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(feature = "gateway-rpc")]
        {
            let rpc = crate::rpc::HttpGatewayRpc::new(nim_rpc_url, crate::NetworkId::Testnet)
                .map_err(|e| LiveSwapFfiError::Refused {
                    reason: e.to_string(),
                })?;
            Ok(Arc::new(NimHtlcRefunder::from_parts(
                nim_key,
                Arc::new(rpc),
            )))
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            let _ = (nim_key, nim_rpc_url);
            Err(LiveSwapFfiError::Unsupported)
        }
    }

    /// The MAINNET refunder — the never-strand door for REAL NIM locks. Deliberately **NOT**
    /// behind the `mainnet_swap` arming switch, unlike every swap constructor: a
    /// `TimeoutResolve` can only ever pay the HTLC's own `htlc_sender` (chain-enforced — this
    /// wallet, the funder), so this door cannot create exposure, only reduce it; and a lock
    /// funded by an ARMED build must stay refundable even if a later build disarms. The url
    /// must be a mainnet RPC (a "testnet"-looking url is refused). Requires `gateway-rpc`.
    #[uniffi::constructor]
    pub fn new_mainnet(
        nim_key: Arc<dyn EnclaveKey>,
        nim_rpc_url: String,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(feature = "gateway-rpc")]
        {
            let rpc = crate::rpc::HttpGatewayRpc::new_mainnet(nim_rpc_url).map_err(|e| {
                LiveSwapFfiError::Refused {
                    reason: e.to_string(),
                }
            })?;
            Ok(Arc::new(NimHtlcRefunder::from_parts_on(
                nim_key,
                Arc::new(rpc),
                crate::NetworkId::Mainnet,
            )))
        }
        #[cfg(not(feature = "gateway-rpc"))]
        {
            let _ = (nim_key, nim_rpc_url);
            Err(LiveSwapFfiError::Unsupported)
        }
    }

    /// Attempt to refund one lock. Idempotent: `StillLocked` before the timeout,
    /// `Refunded { tx_hash }` on a broadcast, `AlreadyResolved` once the contract is empty
    /// (only THAT proves the funds are back — the A2c verified-refund rule).
    pub fn refund(&self, lock: FfiNimLock) -> Result<FfiRefundOutcome, LiveSwapFfiError> {
        let contract = Address::from_user_friendly(&lock.contract)
            .map_err(|e| bad(format!("bad contract address: {e:?}")))?;
        let balance = match self.rpc.get_account(&lock.contract) {
            Err(e) => {
                return Err(LiveSwapFfiError::Transport {
                    reason: e.to_string(),
                })
            }
            Ok(None) => 0,
            Ok(Some(acct)) => acct.balance,
        };
        if balance == 0 {
            return Ok(FfiRefundOutcome::AlreadyResolved);
        }
        let due = lock.timeout_ms.saturating_add(REFUND_MARGIN_MS);
        if (self.clock_ms)() <= due {
            return Ok(FfiRefundOutcome::StillLocked { until_ms: due });
        }
        let pk: [u8; 32] = self
            .key
            .public_key()
            .try_into()
            .map_err(|_| bad("enclave public key is not 32 bytes"))?;
        let head = self
            .rpc
            .block_number()
            .map_err(|e| LiveSwapFfiError::Transport {
                reason: e.to_string(),
            })?;
        let redeem = HtlcRedeem {
            contract,
            // TimeoutResolve pays the HTLC's sender — the wallet that funded it.
            recipient: Address::from_public_key(&pk),
            value: balance, // refund everything the contract still holds
            fee: 0,
            validity_start_height: head,
            network_id: self.network.wire_id(),
        };
        let signature: [u8; 64] = self
            .key
            .sign_content(redeem.serialize_content())
            .try_into()
            .map_err(|_| bad("enclave signature is not 64 bytes"))?;
        let wire = redeem.serialize_wire(&timeout_resolve_proof(&pk, &signature));
        self.rpc
            .send_raw_transaction(&bytes_to_hex(&wire))
            .map_err(|e| LiveSwapFfiError::Transport {
                reason: e.to_string(),
            })?;
        Ok(FfiRefundOutcome::Refunded {
            tx_hash: bytes_to_hex(&redeem.tx_hash()),
        })
    }
}

impl NimHtlcRefunder {
    /// The seam constructor (tests + rigs inject a mock RPC; the FFI ctor injects the
    /// testnet-guarded HTTP client). Testnet wire id — the historical default.
    pub fn from_parts(key: Arc<dyn EnclaveKey>, rpc: Arc<dyn GatewayRpc>) -> Self {
        NimHtlcRefunder::from_parts_on(key, rpc, crate::NetworkId::Testnet)
    }

    /// The network-aware seam constructor: the refund tx is stamped with THIS network's wire
    /// id (a testnet-stamped refund of a mainnet HTLC is unrelayable, and vice versa).
    pub fn from_parts_on(
        key: Arc<dyn EnclaveKey>,
        rpc: Arc<dyn GatewayRpc>,
        network: crate::NetworkId,
    ) -> Self {
        NimHtlcRefunder {
            key,
            rpc,
            network,
            clock_ms: now_ms,
        }
    }

    /// Inject a deterministic clock (tests).
    pub fn with_clock(mut self, clock_ms: fn() -> u64) -> Self {
        self.clock_ms = clock_ms;
        self
    }
}

/// The EVM account address (0x-hex) a 32-byte secp256k1 secret controls — so the app can
/// derive/persist its Amoy gas + receive accounts without shipping EVM math natively.
/// Requires the `polygon-leg` feature (the k256 signer); refuses honestly otherwise.
#[uniffi::export]
pub fn evm_address_for_secret(secret: Vec<u8>) -> Result<String, LiveSwapFfiError> {
    #[cfg(feature = "polygon-leg")]
    {
        let seed: [u8; 32] = secret
            .as_slice()
            .try_into()
            .map_err(|_| bad("secret must be 32 bytes"))?;
        let key = crate::evm_signer::LocalEvmKey::from_secret(&seed)
            .map_err(|_| bad("secret is not a valid secp256k1 scalar"))?;
        Ok(format!("0x{}", bytes_to_hex(&key.address())))
    }
    #[cfg(not(feature = "polygon-leg"))]
    {
        let _ = secret;
        Err(LiveSwapFfiError::Unsupported)
    }
}

// --- the live constructors --------------------------------------------------------------------

#[uniffi::export]
impl MeshNode {
    /// Build the PHONE's live swap participant (NIM-giver / initiator) on Albatross TESTNET
    /// ⇄ Polygon AMOY — real test coins move (module docs). `nim_funding_key` is the wallet's
    /// enclave key (funds + refunds the NIM HTLC; the seed never crosses); `lock_book`
    /// receives every real NIM lock for the refund path; `gateway_rpc_url` optionally makes
    /// this node also a testnet gateway (a phone passes `None` and hears mesh beacons).
    ///
    /// Requires the core built with `polygon-gateway` + `gateway-rpc`; refuses otherwise.
    #[uniffi::constructor]
    pub fn new_live_swap_initiator(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        nim_funding_key: Arc<dyn EnclaveKey>,
        lock_book: Arc<LiveLockBook>,
        config: FfiLiveInitiatorConfig,
        gateway_rpc_url: Option<String>,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
        {
            live_impl::build_initiator(
                sender_id,
                radio,
                nim_funding_key,
                lock_book,
                config,
                gateway_rpc_url,
            )
        }
        #[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
        {
            let _ = (
                sender_id,
                radio,
                nim_funding_key,
                lock_book,
                config,
                gateway_rpc_url,
            );
            Err(LiveSwapFfiError::Unsupported)
        }
    }

    /// Build the MAC RIG's live swap responder (USDC-giver) on TESTNET ⇄ AMOY — real test
    /// coins move (module docs). It advertises "gives USDC, wants NIM", funds NOTHING until
    /// its real `NimHtlcVerifier` sees the initiator's HTLC on-chain at depth, then escrows
    /// the USDC and claims the NIM leg with the revealed `S`.
    ///
    /// Requires the core built with `polygon-gateway` + `gateway-rpc`; refuses otherwise.
    #[uniffi::constructor]
    pub fn new_live_swap_responder(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        config: FfiLiveResponderConfig,
        gateway_rpc_url: Option<String>,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
        {
            live_impl::build_responder(sender_id, radio, config, gateway_rpc_url)
        }
        #[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
        {
            let _ = (sender_id, radio, config, gateway_rpc_url);
            Err(LiveSwapFfiError::Unsupported)
        }
    }

    /// **OWNER-GATED (real money): the MAINNET initiator.** The mirror of
    /// [`Self::new_live_swap_initiator`] on Albatross **mainnet** ⇄ Polygon **mainnet**. It
    /// **REFUSES** with a clear [`LiveSwapFfiError::Refused`] unless the mainnet swap path is fully
    /// armed (`mainnet_swap::MAINNET_SWAP_ENABLED` AND a recorded `MAINNET_HTLC_ADDRESS`) — both
    /// off on any shipped build, so this **always refuses and is inert** until Andjroo's arming PR.
    /// When armed it assembles the mainnet money-path config in CODE (native USDC, chain id `137`,
    /// mainnet confirmation depths, and the hard [`crate::swap_funding_verify::SwapCaps::mainnet_first_swap`]
    /// caps); `config.htlc_address`/`config.usdc_address` are IGNORED — the escrow contract + token
    /// are the code-pinned mainnet constants, never caller-supplied. Pass mainnet RPC urls
    /// (`config.nim_rpc_url` = a Nimiq mainnet RPC; `config.amoy_rpc_url` = an allow-listed Polygon
    /// mainnet RPC). Requires `polygon-gateway` + `gateway-rpc`; refuses otherwise.
    #[uniffi::constructor]
    pub fn new_live_swap_initiator_mainnet(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        nim_funding_key: Arc<dyn EnclaveKey>,
        lock_book: Arc<LiveLockBook>,
        config: FfiLiveInitiatorConfig,
        gateway_rpc_url: Option<String>,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
        {
            live_impl::build_initiator_mainnet(
                sender_id,
                radio,
                nim_funding_key,
                lock_book,
                config,
                gateway_rpc_url,
            )
        }
        #[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
        {
            let _ = (
                sender_id,
                radio,
                nim_funding_key,
                lock_book,
                config,
                gateway_rpc_url,
            );
            Err(LiveSwapFfiError::Unsupported)
        }
    }

    /// **OWNER-GATED (real money): the MAINNET responder.** The mirror of
    /// [`Self::new_live_swap_responder`] on mainnet ⇄ mainnet. It **REFUSES** unless the mainnet
    /// swap path is fully armed (see [`Self::new_live_swap_initiator_mainnet`]) — inert on any
    /// shipped build. When armed it escrows **native Polygon-mainnet USDC**
    /// ([`crate::polygon_gateway::NATIVE_USDC_POLYGON_MAINNET`], code-pinned — `config.usdc_address`
    /// is ignored) in the code-pinned `MAINNET_HTLC_ADDRESS`, funds NOTHING until its mainnet
    /// `NimHtlcVerifier` sees the NIM HTLC on-chain at the mainnet depth, and refuses any swap above
    /// the hard `SwapCaps::mainnet_first_swap` caps. Requires `polygon-gateway` + `gateway-rpc`.
    #[uniffi::constructor]
    pub fn new_live_swap_responder_mainnet(
        sender_id: Vec<u8>,
        radio: Arc<dyn BleRadio>,
        config: FfiLiveResponderConfig,
        gateway_rpc_url: Option<String>,
    ) -> Result<Arc<Self>, LiveSwapFfiError> {
        #[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
        {
            live_impl::build_responder_mainnet(sender_id, radio, config, gateway_rpc_url)
        }
        #[cfg(not(all(feature = "polygon-gateway", feature = "gateway-rpc")))]
        {
            let _ = (sender_id, radio, config, gateway_rpc_url);
            Err(LiveSwapFfiError::Unsupported)
        }
    }
}

// --- the featured guts (behind polygon-gateway + gateway-rpc) ---------------------------------
// Extracted to a sibling file for the 800-line guard; `super` there is this module.
#[cfg(all(feature = "polygon-gateway", feature = "gateway-rpc"))]
#[path = "swap_live_ffi_live_impl.rs"]
pub(crate) mod live_impl;

#[cfg(test)]
#[path = "swap_live_ffi_tests.rs"]
mod tests;
