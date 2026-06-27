//! # bitmesh-core
//!
//! The shared, headless Rust core for **nimiq.bitmesh** — the single source of
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
//! - **G4** — the bitmesh wire packet codec (`nimiqTx` 0x30 / `nimiqTxReceipt` 0x31,
//!   256-byte padding, TTL header).
//! - **G6/G7** — TTL/hop relay, LRU dedup, GCS store-and-forward.
//! - **G8** — gateway broadcast (`sendRawTransaction`), TESTNET-only, money-path / gated.
//!
//! Nothing here ever touches key or seed material; only public, broadcast-safe bytes
//! are modelled at this layer. (Core value #1: non-custodial by construction.)

uniffi::setup_scaffolding!();

// G4: the bitmesh wire protocol — packet model, byte codec (encode/decode + PKCS#7
// padding), and the Nimiq TLV envelope. Pure Rust, non-money-path (opaque bytes only;
// no signing, no broadcast). Lives in dedicated modules to keep this file the thin
// FFI surface; the codec is the canonical wire implementation (LOOP.md).
pub mod codec;
pub mod envelope;
pub mod packet;

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
    // G4: the bitmesh packet codec (encode/decode of nimiqTx 0x30 + nimiqTxReceipt
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
