//! # radio — the `BleRadio` foreign trait (the ADR-0002 byte-stream seam)
//!
//! The single surface the **native BLE shim** must provide. Per **ADR-0002** the radio
//! stays native (Swift `CoreBluetooth` / Kotlin `android.bluetooth.le`); the Rust core
//! owns everything above the byte stream. The two are wired with **UniFFI foreign
//! traits** as two objects pointing at each other:
//!
//! - **`BleRadio`** (this trait) — *foreign*, implemented by the shim. The Rust
//!   [`crate::node::MeshNode`] holds an `Arc<dyn BleRadio>` and only ever calls **out**
//!   to it. The radio never sees a TTL, a packet header, or a tx — only opaque bytes and
//!   connection identities.
//! - **`MeshNode`** — a Rust object the shim calls **in** to on every BLE event.
//!
//! ### The four ADR-0002 callback gotchas this boundary is shaped around
//!
//! - **(b) `send` is fire-and-forget.** It returns `()` immediately; the real
//!   CoreBluetooth / GATT write outcome comes back *asynchronously* via
//!   `MeshNode::on_send_result(peer, ok)`. A `BleRadio` impl must never block in `send`.
//! - **(a) the hot path never re-enters the radio synchronously** — that discipline
//!   lives on the [`crate::node::MeshNode`] side (`on_packet_received` only enqueues).
//! - **(c) every shim callback is wrapped defensively** so a handler error during a
//!   flood burst can't surface `UnexpectedUniFFICallbackError` and abort the process.
//! - **(d) the cross-language refcount cycle is broken by a weak edge:** `MeshNode`
//!   holds the radio strongly; the radio holds the node *weakly* (Swift `weak var`,
//!   Kotlin explicit lifecycle). The pure-Rust [`crate::mock_radio::MockRadio`] models
//!   the same weak edge.
//!
//! A "peer" here is a BLE **connection identity** (a `CBPeripheral`/device UUID string),
//! deliberately opaque to the protocol — the radio routes bytes to a connected peer and
//! is blind to what they mean.

use std::sync::Arc;

/// A BLE connection identity, opaque to the protocol layer.
///
/// On a real device this is the platform's peripheral/device UUID string; in the mock
/// harness it is any stable string label. The protocol's 8-byte `senderID` is a
/// *different* concept that never crosses this seam.
pub type PeerId = String;

/// The native radio surface, implemented by the Swift/Kotlin shim (and, for tests, by
/// the pure-Rust [`crate::mock_radio::MockRadio`]).
///
/// Rust holds an `Arc<dyn BleRadio>` and only calls **out**. All methods are
/// non-blocking and infallible at this boundary: outcomes that can fail (a write) are
/// reported back asynchronously through [`crate::node::MeshNode::on_send_result`].
#[uniffi::export(with_foreign)]
pub trait BleRadio: Send + Sync {
    /// Begin advertising this node as a bitmesh peripheral so neighbors can discover and
    /// connect to it. Idempotent; non-blocking.
    fn start_advertising(&self);

    /// Begin scanning for and connecting to neighboring bitmesh peripherals. Each
    /// resulting connection is reported via `MeshNode::on_peer_connected`. Non-blocking.
    fn start_scanning(&self);

    /// **Fire-and-forget** write of `bytes` to one connected `peer_id` (ADR-0002 gotcha
    /// b). Returns immediately; the real write outcome arrives later via
    /// `MeshNode::on_send_result(peer_id, ok)`. The radio must not block or call back
    /// synchronously.
    fn send(&self, peer_id: String, bytes: Vec<u8>);

    /// Tear down the connection to `peer_id`. Non-blocking; a resulting drop surfaces as
    /// `MeshNode::on_peer_disconnected`.
    fn disconnect(&self, peer_id: String);

    /// Stop advertising and scanning and release the radio. Called when the node is torn
    /// down (the weak edge that breaks the refcount cycle — ADR-0002 gotcha d).
    fn stop(&self);
}

/// Convenience re-export so callers can write `Arc<dyn BleRadio>` without importing both.
pub type SharedRadio = Arc<dyn BleRadio>;
