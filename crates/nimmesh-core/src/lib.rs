//! # nimmesh-core
//!
//! The shared, headless Rust core for **nimiq.nimmesh** — the single source of
//! truth for everything safety- and protocol-critical in the offline Bluetooth-mesh
//! Nimiq wallet. It is consumed by the native iOS (Swift) and Android (Kotlin) apps
//! through **UniFFI**, so the exact same audited logic runs byte-identically on both
//! platforms with no WASM and no consensus client. See `docs/adr/0001`.
//!
//! ## What lives here (by goal)
//!
//! This file (G1) is the **scaffold**: it stands up the crate, the UniFFI surface,
//! and a small but *real* exported API that proves the FFI boundary works and is
//! unit-tested end to end. The protocol- and money-critical layers land in later
//! goals and are marked with `G3:` / `G4:` / `G8:` TODO anchors below — none of that
//! logic is implemented yet.
//!
//! - **G3** — offline Nimiq signing (Ed25519 over `serializeContent()`), TESTNET-only,
//!   money-path / gated.
//! - **G4** — the nimmesh wire packet codec (`nimiqTx` 0x30 / `nimiqTxReceipt` 0x31,
//!   256-byte padding, TTL header).
//! - **G6/G7** — TTL/hop relay, LRU dedup, GCS store-and-forward.
//! - **G8** — gateway broadcast (`sendRawTransaction`), TESTNET-only, money-path / gated.
//!
//! Nothing here ever touches key or seed material; only public, broadcast-safe bytes
//! are modelled at this layer. (Core value #1: non-custodial by construction.)

uniffi::setup_scaffolding!();

// G4: the nimmesh wire protocol — packet model, byte codec (encode/decode + PKCS#7
// padding), and the Nimiq TLV envelope. Pure Rust, non-money-path (opaque bytes only;
// no signing, no broadcast). Lives in dedicated modules to keep this file the thin
// FFI surface; the codec is the canonical wire implementation (LOOP.md).
pub mod codec;
pub mod envelope;
pub mod packet;

// Mesh swap (F2, feat/mesh-swap): the swap wire messages (`0x40`–`0x44`) + their TLV envelope,
// layered on the same codec/relay/store-forward/gateway stack as `nimiqTx`. `swap_wire` is the
// type-length-value codec for propose / accept / funding-proof / preimage-reveal / abort —
// public broadcast-safe data + opaque signed tx blobs only (no keys, no preimage-before-reveal).
// The Nimiq HTLC tx serialization the swap funds/claims with lives in `nimiq::htlc`. See
// docs/swap/SWAP.md; the build contract is docs/swap/SWAP-LOOP.md.
pub mod swap_wire;

// Mesh swap (F3): the swap state machine + the `Δ_safe` timelock-safety gate. `swap` owns the
// clock-free, height-anchored lifecycle (propose → accept → fund → reveal → settle, with a refund
// exit in every funded phase) and `assess_ladder` — the gate that REFUSES to fund a swap whose
// timelocks can't be made safe over a high-latency mesh. Rules only: no keys, no bytes, no radio.
pub mod swap;

// Mesh swap (F4): the chain-agnostic `SwapLeg` seam + a faithful in-memory mock HTLC chain.
// `swap_leg` is the trait each chain implements (NimiqLeg via `nimiq::htlc`; BitcoinLeg gated →
// F5); `MockLeg` enforces the real claim/refund rules (SHA-256 hashlock, claim-before-timeout,
// refund-after-timeout) and reveals the preimage on claim, so the `swap_e2e_tests` can prove the
// atomicity invariant: no one-sided settlement is ever possible.
pub mod swap_leg;

// Mesh swap (F5): the per-chain swap-tx BUILDER seam (distinct from `swap_leg`'s escrow model).
// `swap_builder` is the `LegBuilder` trait each chain implements to PRODUCE the signed bytes a
// swap floods: `NimiqLeg` builds a byte-exact, signed HTLC funding tx via `nimiq::htlc` (the seed
// stays behind the `EnclaveKey` seam); its claim/refund await the gated NIM resolve proof, and
// `BitcoinLeg` is a documented gated stub (real BTC node + funds = needs:owner).
pub mod swap_builder;

// Mesh swap (B1): the Bitcoin counterparty leg, built with `rust-bitcoin` behind the
// `bitcoin-leg` feature (off by default → the core stays lean/WASM-friendly). `btc` owns the HTLC
// redeem script (`OP_SHA256` hashlock — the SAME 32-byte H as `nimiq::htlc`), the P2WSH address,
// and (next) the BIP143 claim/refund signing. Byte-validated vs `bitcoinjs-lib`. Signet; gated
// for mainnet. See docs/swap/BTC-LEG.md.
#[cfg(feature = "bitcoin-leg")]
pub mod btc;

/// Re-export of `rust-bitcoin` (behind `bitcoin-leg`) so tools/examples can build keys, addresses,
/// and txids without a separate dependency declaration.
#[cfg(feature = "bitcoin-leg")]
pub use bitcoin;

// Mesh swap: the REAL Bitcoin swap leg (behind `bitcoin-leg`) — the BTC-native analog of
// `swap_builder::NimiqLeg`. Wraps `btc::BtcHtlcParams` + signs through the `btc::BtcEnclaveKey`
// seam, exposing `htlc_address` / `build_claim` / `build_refund`. Supersedes the BTC half of the
// `swap_builder::BitcoinLeg` gated stub (BTC's UTXO model doesn't fit the NIM-shaped `LegBuilder`).
#[cfg(feature = "bitcoin-leg")]
pub mod swap_btc_leg;

// Mesh swap: the cross-chain swap ORCHESTRATOR (behind `bitcoin-leg`). `swap_engine::SwapEngine`
// folds the `live_cross_chain_swap` example into a first-class, pure (no-network) API the UI + mesh
// drive: it owns the `swap::Swap` state machine + this node's `NimiqLeg` + `BtcSwapLeg`, turning each
// `SwapAction` into a concrete `SwapEffect` (tx bytes to broadcast, or a BTC address to fund). The
// responder reads `S` off the on-chain BTC claim — atomicity stays real. Testnet only; mainnet gated.
#[cfg(feature = "bitcoin-leg")]
pub mod swap_engine;

// Mesh swap: the UniFFI facade over `swap_engine::SwapEngine` (behind `bitcoin-leg`). `swap_ffi`
// exposes a `SwapEngineHandle` UniFFI object (like `nimiq::signer::AppSigner`) so the WebView UI +
// the native mesh shim can drive a NIM⇄BTC swap. The pure engine stays uniffi-free; the facade owns
// interior mutability + the bitcoin-type↔FFI conversions. Keys cross as the `with_foreign` EnclaveKey
// / BtcEnclaveKey traits (seeds never cross FFI). Testnet/signet only; mainnet not offered.
#[cfg(feature = "bitcoin-leg")]
pub mod swap_ffi;

// Mesh swap (B2): the BTC gateway — broadcast + confirmation over a public signet indexer
// (mempool.space), behind the `bitcoin-gateway` feature (adds `ureq`/`serde_json`). The one online
// hop for the BTC leg; signet-only (mainnet base refused). See docs/swap/BTC-LEG.md.
#[cfg(feature = "bitcoin-gateway")]
pub mod btc_gateway;

// G5: the BLE mesh node + its byte-stream radio seam (ADR-0002). The native radio stays
// behind the `BleRadio` foreign trait (`radio`); Rust drives it from `MeshNode` (`node`)
// off a worker thread, speaking the real G4 codec via the relay/gateway/origin `engine`
// (`dedup` is its LRU). `mock_radio` is the pure-Rust virtual-topology test substrate.
// The `kind: mock | real` `provider` bundles a radio + gateway; `gateway` is the online
// hop (mock now; real `sendRawTransaction` is G8). `transport` holds the shared ids.
// Everything stays opaque-bytes only — no signing, no broadcast (G3/G8, money-path).
pub mod dedup;
pub mod engine;
pub mod gateway;
pub mod mock_radio;
pub mod node;
pub mod provider;
pub mod radio;
pub mod transport;

// G8: the one online hop — gateway broadcast (TESTNET-only, money-path). `rpc` is the
// blocking Albatross JSON-RPC seam (`GatewayRpc`): a deterministic `MockRpc` (always
// compiled, keeps `cargo test` offline) plus a real `ureq` HTTP client gated behind the
// `gateway-rpc` feature so the pure-protocol core stays dependency-light + WASM-friendly.
// The `gateway::RpcGateway` validates `networkId` (must be testnet 5) + the validity
// window against the live head, then calls `sendRawTransaction(rawHex)`; on accept it
// floods a `nimiqTxReceipt` back. Every constructor is testnet-guarded — there is no path
// to a mainnet RPC. The live-broadcast tool is `examples/live_testnet_broadcast.rs`.
pub mod rpc;

// G6: relay-engine refinements over the G5 basic TTL/dedup/flood relay. `relay` adds
// degree-adaptive probabilistic flooding (high-degree threshold), injectable relay
// jitter (10–220 ms, real in prod / zero in tests), and the TTL hop-cap transform that
// makes the flood provably loop-free; `fragment` implements the `fragment = 0x20`
// split/reassemble path (128 in-flight, 30 s lifetime) for payloads larger than one BLE
// write. Still opaque-bytes only — no signing, no broadcast (G3/G8, money-path).
pub mod fragment;
pub mod relay;

// G12: hardening — anti-spam / anti-DoS over the existing relay. `ratelimit` is the per-peer
// token-bucket inbound limiter (throttle a flooding peer). It pairs with verify-before-relay
// (`nimiq::verify_basic_wire`, drop junk txs) + stop-after-ACK (don't re-carry a landed tx) in
// the `engine`. Pure, clock-free (worker monotonic clock), no keys/sign/broadcast.
pub mod ratelimit;

// G7: store-and-forward via GCS gossip-sync (PROTOCOL.md "Store-and-forward = GCS
// gossip-sync"). `gcs` is the compact Golomb-Coded-Set membership filter (fpr 0.01,
// ≤ 400 B) a node uses to advertise what it has; `store_forward` is the bounded,
// clock-free recent-packet cache (≤ 1000 / 900 s retention) plus the 30 s maintenance
// scheduler. The `engine` wires them into the `requestSync` (0x21, ttl 0, local-only) →
// `isRSR` unicast catch-up so an out-of-range originator/gateway rejoining within 15 min
// recovers the packets it missed. Opaque-bytes only — no signing/broadcast (G3/G8).
pub mod gcs;
pub mod store_forward;

// G9: the head beacon (`nimiqHeadBeacon = 0x32`) + the validity-window guard / packet GC
// (PROTOCOL.md "Validity window — the relay budget", RISKS.md #1). `beacon` owns the
// clock-free building blocks: the `HeadBeacon` payload codec, the monotonic per-node
// `HeadCache` (freshest head heard; the G3 signer anchors `validityStartHeight` to it), a
// `BeaconScheduler` rate-limiter (a gateway floods at most one beacon per tick, sourcing
// the height from its RPC `block_number`), and `is_expired` — the guard the `engine`
// applies so a node never relays or stores a `nimiqTx` whose ~2 h window has closed.
// Non-money-path: reads a public head height, carries opaque references — no signing/broadcast.
pub mod beacon;

// G15: account balance over the mesh (PROTOCOL.md / Andjroo's feature). `balance` owns the
// `nimiqBalanceQuery` (0x33) + `nimiqBalanceResponse` (0x34) payload codecs and the clock-free
// per-address last-known-balance cache (monotonic by head height, like the G9 `HeadCache`).
// A node floods a query; an internet-bearing gateway answers with the balance it read at a
// head height. The answer is UNVERIFIED/last-known until an accounts-proof binds it to the
// head-beacon hash (follow-up). Read-only public state — no keys, no signing (non-money-path).
pub mod balance;

// G18: "pay me X NIM" request links (non-money-path). `request_uri` is the pure, key-free
// codec for the standard `nimiq:<address>?amount=<NIM>&message=<text>` URI a payee shows as a
// QR so the payer's amount is pre-filled (the fat-finger fix). Symmetric (`parse(build(x)) == x`);
// formats/parses public data only — no signing, no seed. Recent recipients + named contacts are
// local UI state in the app; this owns the one cross-platform-exact piece.
pub mod request_uri;

// G17: two-way settlement closure (non-money-path). `settlement` is the per-node payment
// ledger lifted out of `engine` (size-guard) + generalised to BOTH directions: a sender's
// `Outgoing` (on submit) and a payee's `Incoming` (via `MeshNode::watch_incoming`). The same
// flooded `nimiqTxReceipt` (G8) settles whichever side is watching that txId, so `Pending → ✓
// Settled` / `✗ Failed` closes the "did it land?" loop for sender and receiver alike, even if
// neither was online at submit (G7 store-and-forward carries the receipt). Tracks public
// txId + receipt status only — no keys, relay stays blind. Owns the FFI `PaymentStatus`.
pub mod settlement;

// G16: "will it send?" reachability + the validity-window countdown (non-money-path).
// `reachability` turns the node's public mesh state (peer count + a heard gateway head
// beacon, G9 + whether this node is itself a gateway) into an honest `Reachability`
// (Online → Meshed → Offline), and renders how many blocks/seconds a signed tx has left
// before its validity window closes (+ a near-expiry re-sign nudge). Pure, clock-free,
// key-free signals over existing state; the actual signed-tx queue/auto-send is C1 (money-path).
pub mod reachability;

// G20: good-citizen + battery-aware relay (non-money-path). `citizen` owns the relay
// `RelayPosture` derived from the device battery (Full → Reduced → Frugal → Off: relay
// everything while charging/high, damp when mid, carry only the money path when low, stop
// when critical — being a good citizen never costs the user) and the "you helped N payments
// reach the network" good-citizen counter. The `engine` relay gate consults it; the node
// FFI exposes `set_battery` + `relay_stats`. Reads only the battery + the packet's public
// type, decides whether/how much to rebroadcast opaque bytes — no keys, no payload reads.
pub mod citizen;

// G19: the backup-nudge policy (self-custody protection, non-money-path). `backup` owns a
// pure, clock-free, key-free decision — given the account's public state (backed-up?,
// balance, days unprotected) it returns a `BackupUrgency` (None → Gentle → Important →
// Critical) the native UI maps to the vendored `backup-banner`'s escalating treatment.
// Reads only public account facts; touches no seed, no signing, no network.
pub mod backup;

// G3: offline Nimiq Albatross signing (TESTNET, money-path / Andjroo-gated). `nimiq` is the
// byte-exact transaction signer — `serializeContent` (67 B), the full `Basic`-format wire
// blob (139 B), the `Blake2b-256` tx hash, and the user-friendly `NQ`-IBAN address codec —
// all proven equal to `@nimiq/core` against committed fixtures. It exposes the pluggable
// `KeyOrigin` seam: an app-enclave `AppSigner` (the seed stays behind the `EnclaveKey`
// foreign trait, never crossing FFI) and a `DelegatedSigner` (Nimiq Pay / Hub) foreign
// trait, both emitting a `SignedTransfer` whose `raw_hex` floods the mesh as opaque bytes
// via `MeshNode::submit_signed_transfer`. PURE OFFLINE CRYPTO — no network, no broadcast (G8).
pub mod nimiq;

// G11: optional encrypted memo / 1:1 chat (PROTOCOL.md "Encryption"). `noise` is the
// `Noise_XX_25519_ChaChaPoly_SHA256` mutual-auth, identity-hiding handshake plus the two
// ChaChaPoly cipher states with a 1024-message sliding-window replay guard. The handshake
// identity is a long-term Curve25519 **transport** static keypair — explicitly SEPARATE
// from any Nimiq wallet seed (which does not exist here; G3/G10 own that, money-path) —
// and its SHA-256 is the out-of-band-verifiable fingerprint. An encrypted memo rides a
// `nimiqTx` via the `encMemo` TLV (0x05, `envelope`); a 1:1 chat rides a `noiseEncrypted`
// (0x11, `packet`) message. PURE TRANSPORT-PRIVACY — no signing, no broadcast, no wallet.
pub mod noise;

// G5 headless end-to-end + the four ADR-0002 callback-boundary tests. Internal so they
// can drive crate-private observability hooks; only compiled under `cfg(test)`. The G9
// head-beacon / validity-window-guard e2e tests live in `beacon_e2e_tests`; both suites
// share the wire-frame builders + spy radio in `test_support` (keeps each file < 800 lines).
#[cfg(test)]
mod beacon_e2e_tests;
#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod hardening_e2e_tests;
#[cfg(test)]
mod send_e2e_tests;
#[cfg(test)]
mod swap_e2e_tests;
#[cfg(test)]
mod test_support;

/// The Nimiq network this build is talking to.
///
/// **Testnet by default, money-path-gated** (core value #7): the loop never flips to
/// `Mainnet` — that is Andjroo's switch. The discriminants mirror Albatross network
/// ids so the value can be carried straight into a signed transaction at G3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NetworkId {
    /// Nimiq testnet (Albatross `networkId = 5`). The default for all loop work.
    Testnet,
    /// Nimiq mainnet (Albatross `networkId = 24`). Andjroo-gated; never selected by the loop.
    Mainnet,
}

impl NetworkId {
    /// The on-wire Albatross network-id byte for this network.
    ///
    /// Kept as a plain Rust accessor (not exported) so it can be used internally by
    /// the G3 signer and G4 codec without crossing the FFI boundary.
    pub const fn wire_id(self) -> u8 {
        match self {
            // G3/G4: these are the Albatross network discriminants the signed tx and
            // the packet header will carry. No tx is constructed yet.
            NetworkId::Testnet => 5,
            NetworkId::Mainnet => 24,
        }
    }

    /// Whether this network is safe for the autonomous loop to act on unattended.
    ///
    /// Only testnet is. Mainnet requires explicit Andjroo authorization (LOOP.md
    /// guardrails) before any broadcast can occur at G8.
    pub const fn is_loop_safe(self) -> bool {
        matches!(self, NetworkId::Testnet)
    }
}

/// C1: verify a hex-encoded signed transfer is a well-formed, correctly-signed Nimiq basic
/// transfer (the same content-blind check the relay's spam filter uses, [`nimiq::verify_basic_wire`]).
///
/// Exposed so the native app can **self-test the signing path** — sign a transfer with its
/// Keychain/enclave key, then confirm the Rust core accepts the signature (proving the native
/// Ed25519 signer interoperates byte-for-byte with the on-mesh verifier). Pure, no keys.
#[uniffi::export]
pub fn verify_signed_tx_hex(raw_hex: String) -> bool {
    match crate::nimiq::hex::hex_to_bytes(&raw_hex) {
        Ok(wire) => crate::nimiq::verify_basic_wire(&wire),
        Err(_) => false,
    }
}

/// The semantic version of the core crate, as a string, across the FFI boundary.
///
/// Native apps surface this in their about/debug screens so a tester can confirm
/// which core a given build embeds. Sourced from `CARGO_PKG_VERSION` at compile time.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// The default network the loop builds against: always [`NetworkId::Testnet`].
///
/// Exposed so the apps can show "Testnet" without hard-coding a string, and so the
/// default can never silently drift to mainnet.
#[uniffi::export]
pub fn default_network() -> NetworkId {
    NetworkId::Testnet
}

/// The Albatross network-id byte for a given [`NetworkId`], exported for the apps.
///
/// A tiny *real* function over the exported enum — it proves enums round-trip across
/// the UniFFI boundary in both directions (Swift/Kotlin -> Rust -> scalar back).
#[uniffi::export]
pub fn network_wire_id(network: NetworkId) -> u8 {
    network.wire_id()
}

/// Whether the autonomous loop may act unattended on the given network.
///
/// `true` for testnet, `false` for mainnet. Exposed so a UI can refuse to arm an
/// auto-broadcast path against mainnet without surfacing the gate.
#[uniffi::export]
pub fn network_is_loop_safe(network: NetworkId) -> bool {
    network.is_loop_safe()
}

/// A pure byte round-trip across the FFI boundary: returns its input unchanged.
///
/// This is deliberately trivial — its only job is to prove that opaque binary
/// payloads (`Vec<u8>` <-> `[UInt8]` / `ByteArray`) survive the Swift/Kotlin bridge
/// without truncation or re-encoding. The real ~139-byte signed-tx and 256-byte
/// packet payloads at G3/G4 ride the same `Vec<u8>` path, so this is the canary that
/// the binary boundary itself is sound.
#[uniffi::export]
pub fn echo_bytes(payload: Vec<u8>) -> Vec<u8> {
    // G4: the nimmesh packet codec (encode/decode of nimiqTx 0x30 + nimiqTxReceipt
    // 0x31 frames, 256-byte padding, TTL header) replaces this trivial echo.
    payload
}

// G3: DONE — offline Nimiq signing lives in [`nimiq`]. The `KeyOrigin` seam
//      (`nimiq::AppSigner` over the `EnclaveKey` foreign trait, `nimiq::DelegatedSigner`)
//      produces a `nimiq::SignedTransfer` whose `raw_hex` is submitted to the mesh via
//      `node::MeshNode::submit_signed_transfer`. The seed never crosses this boundary —
//      only a public key + signature leave the enclave. TESTNET-default, Andjroo-gated.
//
// G8: DONE — gateway broadcast lives in [`rpc`] + [`gateway::RpcGateway`]. The engine
//      hands a gateway node the decoded envelope (`gateway::SubmitContext`); `RpcGateway`
//      validates `networkId` (testnet 5) + the validity window against the live head, then
//      calls `sendRawTransaction(rawHex)` over the `rpc::GatewayRpc` seam and floods a
//      `nimiqTxReceipt` (0x31) back. TESTNET-only (every constructor is testnet-guarded);
//      the real HTTP client is behind the `gateway-rpc` feature; `cargo test` runs against
//      `rpc::MockRpc` (offline). The live tool: `examples/live_testnet_broadcast.rs`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_matches_cargo_pkg_version() {
        assert_eq!(core_version(), env!("CARGO_PKG_VERSION"));
        // Sanity: it is a non-empty, dotted semver-ish string.
        assert!(core_version().contains('.'), "version should be dotted");
    }

    #[test]
    fn default_network_is_testnet() {
        // The loop must never default to mainnet (LOOP.md guardrails).
        assert_eq!(default_network(), NetworkId::Testnet);
    }

    #[test]
    fn wire_ids_match_albatross_discriminants() {
        assert_eq!(network_wire_id(NetworkId::Testnet), 5);
        assert_eq!(network_wire_id(NetworkId::Mainnet), 24);
        // The exported helper and the const accessor agree.
        assert_eq!(
            network_wire_id(NetworkId::Testnet),
            NetworkId::Testnet.wire_id()
        );
    }

    #[test]
    fn only_testnet_is_loop_safe() {
        assert!(network_is_loop_safe(NetworkId::Testnet));
        assert!(!network_is_loop_safe(NetworkId::Mainnet));
    }

    #[test]
    fn echo_bytes_is_identity_for_arbitrary_payloads() {
        let empty: Vec<u8> = Vec::new();
        assert_eq!(echo_bytes(empty.clone()), empty);

        // A 139-byte payload — the size of a real signed Nimiq basic transfer — to
        // mirror the actual blob G3 will push through this same boundary.
        let signed_tx_sized: Vec<u8> = (0..139u32).map(|i| (i % 256) as u8).collect();
        assert_eq!(echo_bytes(signed_tx_sized.clone()), signed_tx_sized);

        // Full byte range, to catch any 7-bit / encoding truncation across the bridge.
        let all_bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(echo_bytes(all_bytes.clone()), all_bytes);
    }
}
