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

// G5 headless end-to-end + the four ADR-0002 callback-boundary tests. Internal so they
// can drive crate-private observability hooks; only compiled under `cfg(test)`.
#[cfg(test)]
mod e2e_tests;

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

// G3: `sign_offline(seed_handle, content_bytes) -> proof_bytes` — Ed25519 over the
//      deterministic `serializeContent()` of a testnet transfer. MONEY-PATH, gated;
//      the seed never crosses this boundary, only an opaque enclave handle in / a
//      public proof out. Not implemented.
//
// G8: `gateway_broadcast(raw_hex) -> receipt` — validate (networkId + validity
//      window) then `sendRawTransaction` against a public Albatross testnet RPC.
//      MONEY-PATH, gated. Not implemented.

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
