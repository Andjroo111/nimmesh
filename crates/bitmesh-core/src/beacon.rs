//! # beacon — head-beacon (`0x32`) + the validity-window guard / packet GC (G9)
//!
//! PROTOCOL.md "Validity window — the relay budget" + RISKS.md #1 (the single constraint
//! that most shapes the design): an Albatross tx is valid only for
//! `[validityStartHeight, validityStartHeight + VALIDITY_WINDOW)` (`7200` blocks ≈ 2 h).
//! That window is the **entire mesh relay budget** for a signed tx.
//!
//! This module owns the three clock-free building blocks the [`crate::engine`] wires
//! together, all deterministic (the caller injects the head + logical time):
//!
//! - [`HeadBeacon`] + its [`encode_beacon`] / [`decode_beacon`] codec — the
//!   `nimiqHeadBeacon` (`0x32`) payload `{height u32 | blockHash 32 | networkId 1}` a
//!   **gateway** floods so a deep-offline signer can anchor `validityStartHeight` to the
//!   freshest head it has heard (never a stale one).
//! - [`HeadCache`] — every node caches the **latest** head it has heard (monotonic; an
//!   older beacon is ignored), validated against the node's `networkId`. The G3 signer /
//!   intent path reads it to anchor `validityStartHeight`.
//! - [`BeaconScheduler`] — a rate-limiter (at most one emit per [`BEACON_TICK_MS`]),
//!   identical in spirit to the G7 [`crate::store_forward::SyncScheduler`]: caller-driven,
//!   clock-free, so a burst of polls floods at most one beacon per tick.
//!
//! Plus [`is_expired`] — the validity-window guard the engine applies before relaying or
//! storing a `nimiqTx`: a packet whose window has already closed (`head >= validUntil`) is
//! **dropped** (GC'd), so the mesh never carries a dead tx.
//!
//! **Non-money-path.** Nothing here signs, broadcasts, or touches key material — it reads
//! a public head height and carries opaque references (core value #1). The `blockHash`
//! field is carried for protocol fidelity; G8's RPC seam exposes only `getBlockNumber`
//! (height), so a gateway emits a zeroed hash until a `getBlock` slice exists — the
//! **height is the validity anchor**, the hash is informational.

use std::sync::atomic::Ordering;

use crate::codec::encode;
use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::envelope::{decode_envelope, DEFAULT_NETWORK_ID};
use crate::nimiq::VALIDITY_WINDOW;
use crate::packet::{MessageType, Packet};

/// Length of a Nimiq block hash carried in a beacon (Blake2b-256), in bytes.
pub const BLOCK_HASH_LEN: usize = 32;

/// The exact on-wire length of a [`HeadBeacon`] payload: `height(4) | blockHash(32) |
/// networkId(1)`.
pub const BEACON_PAYLOAD_LEN: usize = 4 + BLOCK_HASH_LEN + 1;

/// The beacon cadence: a gateway floods a fresh head beacon at most once per 10 s. Like the
/// G7 sync tick this is enforced clock-free by [`BeaconScheduler`] against a caller clock.
pub const BEACON_TICK_MS: u64 = 10_000;

/// A decoded `nimiqHeadBeacon` (`0x32`) — a gateway's snapshot of the chain head a node
/// anchors transaction validity to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadBeacon {
    /// The head block height (the validity anchor — `validUntil = height + VALIDITY_WINDOW`).
    pub height: u32,
    /// The head block hash (Blake2b-256). Informational; zeroed until a `getBlock` RPC
    /// slice exists (G8 exposes only `getBlockNumber`).
    pub block_hash: [u8; BLOCK_HASH_LEN],
    /// The Albatross network id this head is for (a node ignores a mismatched beacon).
    pub network_id: u8,
}

impl HeadBeacon {
    /// A beacon for `height` on `network_id` with a zeroed (not-yet-sourced) block hash.
    pub fn new(height: u32, network_id: u8) -> Self {
        HeadBeacon {
            height,
            block_hash: [0u8; BLOCK_HASH_LEN],
            network_id,
        }
    }

    /// The block height past which a tx anchored to this head is dead (`height +
    /// VALIDITY_WINDOW`). Saturating, so a near-`u32::MAX` head can never wrap.
    pub fn valid_until(&self) -> u32 {
        self.height.saturating_add(VALIDITY_WINDOW)
    }
}

/// Serialize a [`HeadBeacon`] to its fixed [`BEACON_PAYLOAD_LEN`]-byte payload (big-endian
/// height || blockHash || networkId).
pub fn encode_beacon(beacon: &HeadBeacon) -> Vec<u8> {
    let mut v = Vec::with_capacity(BEACON_PAYLOAD_LEN);
    v.extend_from_slice(&beacon.height.to_be_bytes());
    v.extend_from_slice(&beacon.block_hash);
    v.push(beacon.network_id);
    v
}

/// Parse a [`HeadBeacon`] payload, returning `None` on any malformed input (the mesh is
/// hostile — a wrong-length beacon is dropped, never guessed at).
pub fn decode_beacon(payload: &[u8]) -> Option<HeadBeacon> {
    if payload.len() != BEACON_PAYLOAD_LEN {
        return None;
    }
    let height = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let mut block_hash = [0u8; BLOCK_HASH_LEN];
    block_hash.copy_from_slice(&payload[4..4 + BLOCK_HASH_LEN]);
    let network_id = payload[BEACON_PAYLOAD_LEN - 1];
    Some(HeadBeacon {
        height,
        block_hash,
        network_id,
    })
}

/// The validity-window guard: is a packet with this `valid_until` already dead relative to
/// the freshest head we have heard?
///
/// Returns `true` only when we know **both** a head and a window and `head >= valid_until`.
/// With no cached head, or a packet that carries no window, the node cannot judge expiry —
/// so it does **not** drop (a node that has heard no beacon must not GC everything).
pub fn is_expired(head: Option<u32>, valid_until: Option<u32>) -> bool {
    match (head, valid_until) {
        (Some(h), Some(vu)) => h >= vu,
        _ => false,
    }
}

/// Every node's cache of the freshest head height it has heard via a beacon.
///
/// **Monotonic:** an older (or equal) beacon is ignored, so a deep-offline signer always
/// anchors to the newest head it has seen — never a stale replay. Beacons for a different
/// `networkId` are rejected outright.
#[derive(Debug, Clone)]
pub struct HeadCache {
    /// The network this node operates on; a beacon for any other network is ignored.
    network_id: u8,
    /// The freshest head height heard so far, or `None` before any valid beacon.
    height: Option<u32>,
}

impl HeadCache {
    /// A cache for a node on `network_id` that has heard no beacon yet.
    pub fn new(network_id: u8) -> Self {
        HeadCache {
            network_id,
            height: None,
        }
    }

    /// Accept a beacon: reject a `networkId` mismatch, ignore a non-newer height, otherwise
    /// advance the cached head. Returns `true` iff the cached head moved forward.
    pub fn accept(&mut self, beacon: &HeadBeacon) -> bool {
        if beacon.network_id != self.network_id {
            return false;
        }
        match self.height {
            Some(h) if beacon.height <= h => false,
            _ => {
                self.height = Some(beacon.height);
                true
            }
        }
    }

    /// The freshest head height heard, or `None` if no valid beacon has arrived.
    pub fn latest(&self) -> Option<u32> {
        self.height
    }

    /// The `validUntil` budget a tx signed against the freshest head would carry
    /// (`latest + VALIDITY_WINDOW`), or `None` if no head has been heard.
    pub fn valid_until(&self) -> Option<u32> {
        self.height.map(|h| h.saturating_add(VALIDITY_WINDOW))
    }
}

impl Default for HeadCache {
    /// A testnet cache (the loop default) with no head heard yet.
    fn default() -> Self {
        HeadCache::new(DEFAULT_NETWORK_ID)
    }
}

/// A clock-free rate-limiter for the periodic head-beacon emit (gateway-only).
///
/// [`due`](BeaconScheduler::due) returns `true` at most once per [`BEACON_TICK_MS`]; the
/// caller passes a monotonic `now_ms`, so a flood of polls emits at most one beacon per
/// tick. Mirrors the G7 [`crate::store_forward::SyncScheduler`].
#[derive(Debug, Default)]
pub struct BeaconScheduler {
    last_ms: Option<u64>,
}

impl BeaconScheduler {
    /// A fresh scheduler that is due on its first poll.
    pub fn new() -> Self {
        BeaconScheduler { last_ms: None }
    }

    /// Whether a beacon emit is due at `now_ms`. The first poll always fires; afterwards it
    /// fires only once every [`BEACON_TICK_MS`]. Records the firing time when it returns
    /// `true`.
    pub fn due(&mut self, now_ms: u64) -> bool {
        match self.last_ms {
            Some(t) if now_ms.saturating_sub(t) < BEACON_TICK_MS => false,
            _ => {
                self.last_ms = Some(now_ms);
                true
            }
        }
    }
}

// --- engine glue (G9 head-beacon over the mesh) --------------------------------
//
// The emit / relay / validity-read functions that wire this beacon codec into the worker
// (`crate::engine`). Kept here for cohesion + to keep `engine.rs` under the 800-line guard
// (mirrors how `balance.rs` / `settlement.rs` own their own engine glue).

/// G9: a **gateway** floods a `nimiqHeadBeacon` (`0x32`) carrying its freshest head height so
/// deep-offline signers anchor `validityStartHeight` to a current head (RISKS.md #1). Sources
/// the height from the gateway's RPC (`block_number`, via [`crate::gateway::MeshGateway::head_beacon`]);
/// rate-limited to one emit per [`BEACON_TICK_MS`] (caller-driven, clock-free). A non-gateway
/// node, or a head-fetch failure, emits nothing. The gateway also caches its own head so its
/// local signer anchors fresh.
pub(crate) fn emit_head_beacon(ctx: &WorkerCtx, st: &mut WorkerState) {
    let Some(gateway) = ctx.gateway.as_ref() else {
        return; // only a gateway (internet + RPC) beacons heads.
    };
    let now = st.now_ms();
    if !st.beacon.due(now) {
        return; // rate-limited: at most one beacon per tick.
    }
    let Some(beacon) = gateway.head_beacon() else {
        return; // no live head (transient RPC failure / chain-less mock): emit nothing.
    };
    ctx.cache_head(&beacon);
    let packet = ctx.build_packet(MessageType::NimiqHeadBeacon, encode_beacon(&beacon));
    // Remember our own beacon so its echo back through the mesh is not re-flooded.
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    ctx.beacon_emitted.fetch_add(1, Ordering::Relaxed);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// G9: handle an inbound `nimiqHeadBeacon` (`0x32`). Blind-dedup, cache the freshest head
/// (monotonic; `networkId`-validated), remember it for store-and-forward, then flood it onward
/// so deep-offline nodes downstream also anchor a current head.
pub(crate) fn handle_head_beacon(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    if let Some(beacon) = decode_beacon(&packet.payload) {
        ctx.cache_head(&beacon);
    }
    remember(ctx, st, &packet);
    relay_onward(ctx, packet, src, st);
}

/// G9: the optional `validUntil` height a `nimiqTx` packet carries in its envelope — the input
/// to the [`is_expired`] guard. Returns `None` for any non-decodable payload or a tx without a
/// window (the relay stays blind to the opaque `txWire`; it reads only the public envelope
/// field that bounds the relay budget).
pub(crate) fn tx_valid_until(packet: &Packet) -> Option<u32> {
    decode_envelope(&packet.payload).ok()?.valid_until
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_payload_round_trips() {
        let mut beacon = HeadBeacon::new(1_234_567, DEFAULT_NETWORK_ID);
        beacon.block_hash = [0xAB; BLOCK_HASH_LEN];
        let bytes = encode_beacon(&beacon);
        assert_eq!(bytes.len(), BEACON_PAYLOAD_LEN);
        assert_eq!(decode_beacon(&bytes), Some(beacon));
    }

    #[test]
    fn decode_beacon_rejects_wrong_length() {
        assert!(decode_beacon(&[]).is_none());
        assert!(decode_beacon(&[0u8; BEACON_PAYLOAD_LEN - 1]).is_none());
        assert!(decode_beacon(&[0u8; BEACON_PAYLOAD_LEN + 1]).is_none());
    }

    #[test]
    fn valid_until_is_head_plus_window_and_saturates() {
        assert_eq!(HeadBeacon::new(100, 5).valid_until(), 100 + VALIDITY_WINDOW);
        // Near the u32 ceiling the window can never wrap around.
        assert_eq!(HeadBeacon::new(u32::MAX, 5).valid_until(), u32::MAX);
    }

    #[test]
    fn head_cache_is_monotonic_and_ignores_older() {
        let mut cache = HeadCache::new(DEFAULT_NETWORK_ID);
        assert_eq!(cache.latest(), None);
        // First beacon sets the head.
        assert!(cache.accept(&HeadBeacon::new(100, DEFAULT_NETWORK_ID)));
        assert_eq!(cache.latest(), Some(100));
        // A newer beacon advances it.
        assert!(cache.accept(&HeadBeacon::new(150, DEFAULT_NETWORK_ID)));
        assert_eq!(cache.latest(), Some(150));
        // An older beacon is ignored (monotonic) …
        assert!(!cache.accept(&HeadBeacon::new(120, DEFAULT_NETWORK_ID)));
        assert_eq!(cache.latest(), Some(150));
        // … as is an equal one.
        assert!(!cache.accept(&HeadBeacon::new(150, DEFAULT_NETWORK_ID)));
        assert_eq!(cache.latest(), Some(150));
    }

    #[test]
    fn head_cache_rejects_wrong_network() {
        let mut cache = HeadCache::new(DEFAULT_NETWORK_ID);
        // A mainnet (24) beacon on a testnet (5) node is ignored.
        assert!(!cache.accept(&HeadBeacon::new(999, 24)));
        assert_eq!(cache.latest(), None);
        // The matching-network beacon is accepted.
        assert!(cache.accept(&HeadBeacon::new(999, DEFAULT_NETWORK_ID)));
        assert_eq!(cache.latest(), Some(999));
    }

    #[test]
    fn head_cache_valid_until_tracks_latest() {
        let mut cache = HeadCache::new(DEFAULT_NETWORK_ID);
        assert_eq!(cache.valid_until(), None);
        cache.accept(&HeadBeacon::new(7000, DEFAULT_NETWORK_ID));
        assert_eq!(cache.valid_until(), Some(7000 + VALIDITY_WINDOW));
    }

    #[test]
    fn is_expired_needs_both_head_and_window() {
        // head past the window -> expired.
        assert!(is_expired(Some(1000), Some(1000)));
        assert!(is_expired(Some(1001), Some(1000)));
        // head still inside the window -> alive.
        assert!(!is_expired(Some(999), Some(1000)));
        // missing either side -> cannot judge -> not expired (never GC blindly).
        assert!(!is_expired(None, Some(1000)));
        assert!(!is_expired(Some(1000), None));
        assert!(!is_expired(None, None));
    }

    #[test]
    fn beacon_scheduler_fires_once_per_tick() {
        let mut s = BeaconScheduler::new();
        assert!(s.due(0)); // first poll fires.
        assert!(!s.due(1_000)); // too soon.
        assert!(!s.due(BEACON_TICK_MS - 1));
        assert!(s.due(BEACON_TICK_MS)); // a full tick later — fires again.
        assert!(!s.due(BEACON_TICK_MS + 1));
    }
}
