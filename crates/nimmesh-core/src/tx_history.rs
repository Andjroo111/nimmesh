//! # tx_history — transaction history over the mesh (`0x35`/`0x36`, the G15 pattern)
//!
//! Andjroo's field gap from the live mainnet mesh test: a Bluetooth-only phone could see
//! its **balance** move (G15) but never the transaction **rows** — the history list is
//! RPC-fed and froze offline. This module extends the balance-over-mesh pattern to the
//! recent history: a node floods a `nimiqTxHistoryQuery` (`0x35`, address only) and any
//! internet-bearing gateway answers a `nimiqTxHistoryResponse` (`0x36`) carrying up to
//! [`HISTORY_MAX`] compact records read from its RPC at the current head.
//!
//! The response (~700 B for 10 records) exceeds a single BLE frame, so the gateway
//! floods it through the G6 fragmenter (`fragment = 0x20`, chunks sized to the proven
//! 256-byte frame class) — the first real user of the outbound fragmentation path; the
//! receiver's [`crate::fragment::Reassembler`] (already wired in the engine) rebuilds it
//! and dispatches to [`handle_history_response`] like any other packet.
//!
//! Same trust model as G15: **unverified / last-known** — a gateway is answering from
//! public chain state at a stated head height; the cache is monotonic by that head and
//! rejects a `networkId` mismatch against THIS node's network. Read-only end to end —
//! addresses and public transactions only, no keys, and the relay stays blind (relays
//! carry both packet types as opaque payloads).

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::codec::encode;
use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::fragment::fragment_message;
use crate::nimiq::address::{Address, ADDRESS_LEN};
use crate::node::MeshNode;
use crate::packet::{MessageType, Packet};

/// Max records a response carries — enough for the app's history screen, small enough
/// to stay a handful of fragments.
pub const HISTORY_MAX: usize = 10;

/// Wire length of one [`TxHistoryRecord`]: `hash(32) | counterparty(20) | value(8) |
/// timestamp_ms(8) | flags(1)`.
pub const HISTORY_RECORD_LEN: usize = 32 + ADDRESS_LEN + 8 + 8 + 1;

/// Wire length of a query payload (the address).
pub const HISTORY_QUERY_LEN: usize = ADDRESS_LEN;

/// Fixed response header: `address(20) | headHeight(4) | networkId(1) | count(1)`.
pub const HISTORY_RESPONSE_HEADER_LEN: usize = ADDRESS_LEN + 4 + 1 + 1;

/// Fragment chunk size for the response flood: keeps every fragment packet inside the
/// proven 256-byte padded frame class (header ~30 B + fragment header 13 B + margin).
pub const HISTORY_FRAGMENT_CHUNK: usize = 180;

/// `flags` bit: the tx pays INTO the queried address.
pub const FLAG_INCOMING: u8 = 0b0000_0001;
/// `flags` bit: the tx is included in a block (vs mempool-pending).
pub const FLAG_CONFIRMED: u8 = 0b0000_0010;

/// One compact history row, exactly what the app's list needs (G15 trust model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxHistoryRecord {
    /// The transaction hash (32 bytes).
    pub hash: [u8; 32],
    /// The OTHER party (sender for incoming, recipient for outgoing) — the queried
    /// address's perspective is baked in by the answering gateway via [`FLAG_INCOMING`].
    pub counterparty: Address,
    /// Value in luna.
    pub value: u64,
    /// The chain timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// [`FLAG_INCOMING`] | [`FLAG_CONFIRMED`].
    pub flags: u8,
}

impl TxHistoryRecord {
    /// Whether the tx pays INTO the queried address.
    pub fn incoming(&self) -> bool {
        self.flags & FLAG_INCOMING != 0
    }
    /// Whether the tx is included in a block.
    pub fn confirmed(&self) -> bool {
        self.flags & FLAG_CONFIRMED != 0
    }
}

/// A gateway → mesh history answer for one address at one head height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryResponse {
    /// The queried address (response ↔ query matching).
    pub address: Address,
    /// The chain head the gateway read at — the freshness anchor (monotonic cache key).
    pub head_height: u32,
    /// The Albatross network-id byte the gateway is on; a node rejects a mismatch.
    pub network_id: u8,
    /// Newest-first, at most [`HISTORY_MAX`].
    pub records: Vec<TxHistoryRecord>,
}

/// Encode a query payload (20 bytes — just the address).
pub fn encode_history_query(address: &Address) -> Vec<u8> {
    address.as_bytes().to_vec()
}

/// Decode a query payload. `None` unless exactly [`HISTORY_QUERY_LEN`].
pub fn decode_history_query(bytes: &[u8]) -> Option<Address> {
    if bytes.len() != HISTORY_QUERY_LEN {
        return None;
    }
    let mut addr = [0u8; ADDRESS_LEN];
    addr.copy_from_slice(bytes);
    Some(Address::from_bytes(addr))
}

/// Encode a response payload (header + `count` fixed-width records, big-endian).
pub fn encode_history_response(r: &HistoryResponse) -> Vec<u8> {
    let count = r.records.len().min(HISTORY_MAX);
    let mut out = Vec::with_capacity(HISTORY_RESPONSE_HEADER_LEN + count * HISTORY_RECORD_LEN);
    out.extend_from_slice(r.address.as_bytes());
    out.extend_from_slice(&r.head_height.to_be_bytes());
    out.push(r.network_id);
    out.push(count as u8);
    for rec in r.records.iter().take(count) {
        out.extend_from_slice(&rec.hash);
        out.extend_from_slice(rec.counterparty.as_bytes());
        out.extend_from_slice(&rec.value.to_be_bytes());
        out.extend_from_slice(&rec.timestamp_ms.to_be_bytes());
        out.push(rec.flags);
    }
    out
}

/// Decode a response payload. `None` on any malformed input (wrong header, a count
/// beyond [`HISTORY_MAX`], or a length that doesn't match the count exactly).
pub fn decode_history_response(bytes: &[u8]) -> Option<HistoryResponse> {
    if bytes.len() < HISTORY_RESPONSE_HEADER_LEN {
        return None;
    }
    let mut addr = [0u8; ADDRESS_LEN];
    addr.copy_from_slice(&bytes[..ADDRESS_LEN]);
    let head_height = u32::from_be_bytes(bytes[ADDRESS_LEN..ADDRESS_LEN + 4].try_into().ok()?);
    let network_id = bytes[ADDRESS_LEN + 4];
    let count = bytes[ADDRESS_LEN + 5] as usize;
    if count > HISTORY_MAX
        || bytes.len() != HISTORY_RESPONSE_HEADER_LEN + count * HISTORY_RECORD_LEN
    {
        return None;
    }
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let at = HISTORY_RESPONSE_HEADER_LEN + i * HISTORY_RECORD_LEN;
        let rec = &bytes[at..at + HISTORY_RECORD_LEN];
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&rec[..32]);
        let mut cp = [0u8; ADDRESS_LEN];
        cp.copy_from_slice(&rec[32..32 + ADDRESS_LEN]);
        records.push(TxHistoryRecord {
            hash,
            counterparty: Address::from_bytes(cp),
            value: u64::from_be_bytes(rec[52..60].try_into().ok()?),
            timestamp_ms: u64::from_be_bytes(rec[60..68].try_into().ok()?),
            flags: rec[68],
        });
    }
    Some(HistoryResponse {
        address: Address::from_bytes(addr),
        head_height,
        network_id,
        records,
    })
}

/// Every node's cache of the freshest history heard per address. Monotonic by the
/// response's `head_height`; a response for another network is rejected outright
/// (validated against THIS node's network, same policy as the head cache).
#[derive(Debug, Default)]
pub struct HistoryCache {
    entries: HashMap<Address, HistoryResponse>,
}

impl HistoryCache {
    /// Accept a response for a node on `network_id`: reject a network mismatch, ignore a
    /// non-newer head, otherwise store. Returns whether the cache advanced.
    pub fn observe(&mut self, resp: &HistoryResponse, network_id: u8) -> bool {
        if resp.network_id != network_id {
            return false;
        }
        match self.entries.get(&resp.address) {
            Some(have) if resp.head_height <= have.head_height => false,
            _ => {
                self.entries.insert(resp.address, resp.clone());
                true
            }
        }
    }

    /// The freshest history heard for `address`, or `None`.
    pub fn get(&self, address: &Address) -> Option<&HistoryResponse> {
        self.entries.get(address)
    }
}

// --- engine glue (mirrors balance.rs: query flood / gateway answer / response cache) ---

/// Flood this node's own `nimiqTxHistoryQuery` (worker thread; the FFI enqueues).
pub(crate) fn flood_local_history_query(ctx: &WorkerCtx, address: Address, st: &mut WorkerState) {
    let packet = ctx.build_packet(
        MessageType::NimiqTxHistoryQuery,
        encode_history_query(&address),
    );
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// Handle an inbound `nimiqTxHistoryQuery` (`0x35`): dedup + remember + relay onward. A
/// **gateway** additionally reads the recent history via
/// [`crate::gateway::MeshGateway::history_of`] and floods the response — through the G6
/// fragmenter, since ~10 records exceed one BLE frame. A non-gateway just relays.
pub(crate) fn handle_history_query(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    remember(ctx, st, &packet);
    if let Some(gw) = &ctx.gateway {
        if let Some(address) = decode_history_query(&packet.payload) {
            if let Some(resp) = gw.history_of(&address.to_user_friendly()) {
                // The gateway knows its own answer too (same as G15's balance path).
                ctx.cache_history(&resp);
                let payload = encode_history_response(&resp);
                // Derive a unique fragment id from this node's identity + packet
                // sequence (no RNG in the core; uniqueness is what matters).
                let seq = ctx.next_seq();
                let mut frag_id = [0u8; 8];
                frag_id.copy_from_slice(&seq.to_be_bytes());
                for (i, b) in ctx.sender_id.iter().enumerate().take(8) {
                    frag_id[i] ^= *b;
                }
                for frag_payload in fragment_message(
                    MessageType::NimiqTxHistoryResponse.to_u8(),
                    &payload,
                    HISTORY_FRAGMENT_CHUNK,
                    frag_id,
                ) {
                    let frag = ctx.build_packet(MessageType::Fragment, frag_payload);
                    st.relay_seen.insert(relay_key(&frag));
                    remember(ctx, st, &frag);
                    if let Ok(bytes) = encode(&frag) {
                        ctx.flood(bytes);
                    }
                }
                ctx.history_answered.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    relay_onward(ctx, packet, src, st);
}

/// Handle an inbound (reassembled) `nimiqTxHistoryResponse` (`0x36`): dedup, cache
/// (monotonic + network-validated), remember, relay onward.
pub(crate) fn handle_history_response(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    if let Some(resp) = decode_history_response(&packet.payload) {
        ctx.cache_history(&resp);
    }
    remember(ctx, st, &packet);
    relay_onward(ctx, packet, src, st);
}

// --- FFI surface (a second `#[uniffi::export]` block on MeshNode, like gateway_ffi) ---

/// One history row across FFI, shaped for the app's list (hex/user-friendly strings).
#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiTxRecord {
    /// The tx hash, lowercase hex.
    pub hash: String,
    /// The counterparty in user-friendly `NQ…` form.
    pub counterparty: String,
    /// Value in luna.
    pub value_luna: u64,
    /// Chain timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Whether the tx pays INTO the queried address.
    pub incoming: bool,
    /// Whether the tx is included in a block.
    pub confirmed: bool,
    /// The chain head the answering gateway read at (freshness stamp).
    pub head_height: u32,
}

#[uniffi::export]
impl MeshNode {
    /// Ask the mesh for `address`'s recent transactions — flood a `nimiqTxHistoryQuery`
    /// that any internet-bearing gateway answers. Fire-and-forget (non-blocking enqueue);
    /// read the answer with [`Self::cached_tx_history`]. Public address only, no keys.
    pub fn query_tx_history(&self, address: String) {
        if let Ok(addr) = Address::from_user_friendly(&address) {
            self.enqueue_history_query(addr);
        }
    }

    /// The freshest history heard over the mesh for `address` (empty until a gateway has
    /// answered). **Unverified / last-known**, same trust model as the mesh balance.
    pub fn cached_tx_history(&self, address: String) -> Vec<FfiTxRecord> {
        let Ok(addr) = Address::from_user_friendly(&address) else {
            return Vec::new();
        };
        let Some(resp) = self.ctx.cached_history(&addr) else {
            return Vec::new();
        };
        resp.records
            .iter()
            .map(|r| FfiTxRecord {
                hash: crate::nimiq::hex::bytes_to_hex(&r.hash),
                counterparty: r.counterparty.to_user_friendly(),
                value_luna: r.value,
                timestamp_ms: r.timestamp_ms,
                incoming: r.incoming(),
                confirmed: r.confirmed(),
                head_height: resp.head_height,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from_bytes([b; ADDRESS_LEN])
    }

    fn rec(n: u8) -> TxHistoryRecord {
        TxHistoryRecord {
            hash: [n; 32],
            counterparty: addr(n),
            value: n as u64 * 1_000,
            timestamp_ms: n as u64 * 777,
            flags: if n % 2 == 0 {
                FLAG_INCOMING | FLAG_CONFIRMED
            } else {
                FLAG_CONFIRMED
            },
        }
    }

    fn resp(head: u32, network: u8, n: usize) -> HistoryResponse {
        HistoryResponse {
            address: addr(9),
            head_height: head,
            network_id: network,
            records: (0..n as u8).map(rec).collect(),
        }
    }

    #[test]
    fn query_round_trips() {
        let a = addr(7);
        assert_eq!(decode_history_query(&encode_history_query(&a)), Some(a));
        assert_eq!(decode_history_query(&[0u8; 19]), None);
    }

    #[test]
    fn response_round_trips_full_and_empty() {
        for n in [0usize, 1, HISTORY_MAX] {
            let r = resp(1_234, 24, n);
            let back = decode_history_response(&encode_history_response(&r)).unwrap();
            assert_eq!(back, r);
            assert_eq!(back.records.len(), n);
        }
    }

    #[test]
    fn response_decode_rejects_malformed() {
        let mut good = encode_history_response(&resp(10, 5, 3));
        assert!(decode_history_response(&good[..good.len() - 1]).is_none()); // truncated
        good.push(0); // trailing garbage
        assert!(decode_history_response(&good).is_none());
        let mut bad_count = encode_history_response(&resp(10, 5, 1));
        bad_count[HISTORY_RESPONSE_HEADER_LEN - 1] = (HISTORY_MAX + 1) as u8;
        assert!(decode_history_response(&bad_count).is_none());
    }

    #[test]
    fn record_flags_read_back() {
        let r = rec(2);
        assert!(r.incoming() && r.confirmed());
        let r = rec(3);
        assert!(!r.incoming() && r.confirmed());
    }

    #[test]
    fn cache_is_monotonic_and_network_guarded() {
        let mut c = HistoryCache::default();
        assert!(c.observe(&resp(100, 24, 2), 24));
        assert!(!c.observe(&resp(100, 24, 3), 24)); // same head: ignored
        assert!(!c.observe(&resp(90, 24, 3), 24)); // older head: ignored
        assert!(!c.observe(&resp(200, 5, 3), 24)); // wrong network: rejected
        assert!(c.observe(&resp(200, 24, 3), 24)); // newer head: advances
        assert_eq!(c.get(&addr(9)).unwrap().records.len(), 3);
    }
}
