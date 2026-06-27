//! # balance — account balance over the mesh (G15, part 1: wire format + cache)
//!
//! Andjroo's feature: get an account's balance even with no internet, by asking the mesh.
//! A node floods a **balance query** (`nimiqBalanceQuery` `0x33`, just the 20-byte address);
//! any internet-bearing **gateway** answers with a **balance response**
//! (`nimiqBalanceResponse` `0x34`, the balance it read at a head height). Like a `nimiqTx`,
//! these flood and relay over the existing engine — this module owns only the **payload
//! codecs** and the per-address **freshness cache**; the gateway-answers / node-caches
//! wiring is part 2.
//!
//! ## Trust (honest, by construction)
//!
//! A relay is untrusted, so a balance response is **unverified / last-known** — it is the
//! balance *as a gateway claims it* at `head_height`. The UI must surface it as such (with a
//! "synced X ago" freshness stamp derived from `head_height` vs the node's head beacon, G9).
//! A trustless upgrade — an Albatross **accounts-proof** verified against the head-beacon
//! block hash — is the planned follow-up (G15 part 3); this layer carries `head_height` so
//! that proof can bind to it. **Non-money-path:** read-only public state, no keys, no signing.
//!
//! ## On-wire payloads (big-endian, exact-length)
//!
//! ```text
//! BalanceQuery    : address(20)                                  = 20 bytes
//! BalanceResponse : address(20) | balance(8) | headHeight(4) | networkId(1) = 33 bytes
//! ```
//! Decoders are panic-free and length-exact: mesh input is hostile (GOAL.md value #6).

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use crate::codec::encode;
use crate::engine::{relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::nimiq::address::{Address, ADDRESS_LEN};
use crate::packet::{MessageType, Packet};

/// Exact on-wire length of a [`BalanceQuery`] payload.
pub const BALANCE_QUERY_LEN: usize = ADDRESS_LEN; // 20
/// Exact on-wire length of a [`BalanceResponse`] payload.
pub const BALANCE_RESPONSE_LEN: usize = ADDRESS_LEN + 8 + 4 + 1; // 33

/// A mesh → gateway request for an address's on-chain balance (`nimiqBalanceQuery` `0x33`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceQuery {
    /// The address whose balance is being asked for.
    pub address: Address,
}

impl BalanceQuery {
    /// A query for `address`.
    pub fn new(address: Address) -> Self {
        BalanceQuery { address }
    }
}

/// A gateway → mesh answer (`nimiqBalanceResponse` `0x34`): the balance the gateway read
/// for `address` at chain head `head_height` on `network_id`. **Unverified** until an
/// accounts-proof (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceResponse {
    /// The queried address (so a node can match the response to its query).
    pub address: Address,
    /// Balance in luna (1 NIM = 100_000 luna), as the gateway read it.
    pub balance: u64,
    /// The chain head height the gateway read the balance at — the freshness anchor.
    pub head_height: u32,
    /// The Albatross network-id byte the gateway is on (testnet `5`); a node rejects a mismatch.
    pub network_id: u8,
}

/// Encode a [`BalanceQuery`] payload (20 bytes).
pub fn encode_balance_query(q: &BalanceQuery) -> Vec<u8> {
    q.address.as_bytes().to_vec()
}

/// Decode a [`BalanceQuery`] payload. `None` unless the input is exactly [`BALANCE_QUERY_LEN`].
pub fn decode_balance_query(bytes: &[u8]) -> Option<BalanceQuery> {
    if bytes.len() != BALANCE_QUERY_LEN {
        return None;
    }
    let mut addr = [0u8; ADDRESS_LEN];
    addr.copy_from_slice(bytes);
    Some(BalanceQuery::new(Address::from_bytes(addr)))
}

/// Encode a [`BalanceResponse`] payload (33 bytes, big-endian).
pub fn encode_balance_response(r: &BalanceResponse) -> Vec<u8> {
    let mut out = Vec::with_capacity(BALANCE_RESPONSE_LEN);
    out.extend_from_slice(r.address.as_bytes());
    out.extend_from_slice(&r.balance.to_be_bytes());
    out.extend_from_slice(&r.head_height.to_be_bytes());
    out.push(r.network_id);
    out
}

/// Decode a [`BalanceResponse`] payload. `None` unless exactly [`BALANCE_RESPONSE_LEN`].
pub fn decode_balance_response(bytes: &[u8]) -> Option<BalanceResponse> {
    if bytes.len() != BALANCE_RESPONSE_LEN {
        return None;
    }
    let mut addr = [0u8; ADDRESS_LEN];
    addr.copy_from_slice(&bytes[..ADDRESS_LEN]);
    let mut bal = [0u8; 8];
    bal.copy_from_slice(&bytes[ADDRESS_LEN..ADDRESS_LEN + 8]);
    let mut h = [0u8; 4];
    h.copy_from_slice(&bytes[ADDRESS_LEN + 8..ADDRESS_LEN + 12]);
    Some(BalanceResponse {
        address: Address::from_bytes(addr),
        balance: u64::from_be_bytes(bal),
        head_height: u32::from_be_bytes(h),
        network_id: bytes[ADDRESS_LEN + 12],
    })
}

/// What a node remembers for an address: the last-known balance + the head it was read at.
/// FFI-visible (`uniffi::Record`) so the app can show the balance + a "synced X ago" stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CachedBalance {
    /// Balance in luna, as last reported by a gateway (**unverified**).
    pub balance: u64,
    /// The chain head height that balance was read at (drives the "synced X ago" stamp).
    pub head_height: u32,
    /// The network the reporting gateway was on.
    pub network_id: u8,
}

/// A clock-free, per-address last-known-balance cache (G15). Like the G9 `HeadCache`, it is
/// **monotonic by `head_height`**: a response that is not strictly fresher than what we hold
/// is ignored, so a stale/replayed response can never roll a balance backwards. A
/// `network_id` mismatch is rejected outright.
#[derive(Debug, Clone, Default)]
pub struct BalanceCache {
    network_id: Option<u8>,
    entries: HashMap<[u8; ADDRESS_LEN], CachedBalance>,
}

impl BalanceCache {
    /// An empty cache that accepts any network (the first response observed pins it).
    pub fn new() -> Self {
        BalanceCache {
            network_id: None,
            entries: HashMap::new(),
        }
    }

    /// An empty cache pinned to `network_id`: responses on any other network are rejected.
    pub fn for_network(network_id: u8) -> Self {
        BalanceCache {
            network_id: Some(network_id),
            entries: HashMap::new(),
        }
    }

    /// Observe a balance response. Returns `true` if it updated the cache (accepted as fresh),
    /// `false` if rejected as stale (not newer than what we hold) or on a `network_id` mismatch.
    pub fn observe(&mut self, resp: &BalanceResponse) -> bool {
        match self.network_id {
            Some(n) if n != resp.network_id => return false, // wrong network — reject.
            None => self.network_id = Some(resp.network_id), // pin on first sight.
            _ => {}
        }
        let key = *resp.address.as_bytes();
        if let Some(existing) = self.entries.get(&key) {
            // Monotonic: only a strictly fresher head replaces what we hold.
            if resp.head_height <= existing.head_height {
                return false;
            }
        }
        self.entries.insert(
            key,
            CachedBalance {
                balance: resp.balance,
                head_height: resp.head_height,
                network_id: resp.network_id,
            },
        );
        true
    }

    /// The last-known balance for `address`, if any has been observed.
    pub fn get(&self, address: &Address) -> Option<CachedBalance> {
        self.entries.get(address.as_bytes()).copied()
    }

    /// How many distinct addresses have a cached balance.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no balances.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// --- engine glue: query/answer/cache over the mesh (G15 part 2) ----------------------
// These run on the engine worker thread and use a few `pub(crate)` engine internals
// (`build_packet`/`flood`/`relay_key`/`remember`/`relay_onward`), keeping all balance-over-
// mesh logic in one module while the orchestrator (`engine`) stays the dispatcher.

/// A node floods a `nimiqBalanceQuery` (`0x33`) asking any gateway for `address`'s balance.
/// Builds the query, caches it for store-and-forward, and floods it. Read-only — a public
/// address only, no keys.
pub(crate) fn flood_local_balance_query(ctx: &WorkerCtx, address: Address, st: &mut WorkerState) {
    let packet = ctx.build_packet(
        MessageType::NimiqBalanceQuery,
        encode_balance_query(&BalanceQuery::new(address)),
    );
    st.relay_seen.insert(relay_key(&packet));
    remember(ctx, st, &packet);
    if let Ok(bytes) = encode(&packet) {
        ctx.flood(bytes);
    }
}

/// Handle an inbound `nimiqBalanceQuery` (`0x33`): blind-dedup + remember + relay onward (so
/// other gateways can also answer). A **gateway** additionally reads the balance via
/// [`crate::gateway::MeshGateway::balance_of`] (read-only public state), caches its own answer, and floods a
/// `nimiqBalanceResponse` (`0x34`). A non-gateway just relays.
pub(crate) fn handle_balance_query(
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
        if let Some(query) = decode_balance_query(&packet.payload) {
            if let Some(answer) = gw.balance_of(&query.address.to_user_friendly()) {
                let resp = BalanceResponse {
                    address: query.address,
                    balance: answer.balance,
                    head_height: answer.head_height,
                    network_id: answer.network_id,
                };
                ctx.cache_balance(&resp); // the gateway knows its own answer too.
                let reply = ctx.build_packet(
                    MessageType::NimiqBalanceResponse,
                    encode_balance_response(&resp),
                );
                st.relay_seen.insert(relay_key(&reply));
                remember(ctx, st, &reply);
                ctx.balance_answered.fetch_add(1, Ordering::Relaxed);
                if let Ok(bytes) = encode(&reply) {
                    ctx.flood(bytes);
                }
            }
        }
    }
    relay_onward(ctx, packet, src, st);
}

/// Handle an inbound `nimiqBalanceResponse` (`0x34`): blind-dedup, cache the (unverified /
/// last-known) balance (monotonic by head height; `networkId`-guarded), remember it for
/// store-and-forward, then flood it onward so a node that queried while out of range still
/// receives it. Read-only — no keys, no signing.
pub(crate) fn handle_balance_response(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    if let Some(resp) = decode_balance_response(&packet.payload) {
        ctx.cache_balance(&resp);
    }
    remember(ctx, st, &packet);
    relay_onward(ctx, packet, src, st);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from_bytes([b; ADDRESS_LEN])
    }

    #[test]
    fn query_roundtrip_is_exact_length() {
        let q = BalanceQuery::new(addr(0xAB));
        let bytes = encode_balance_query(&q);
        assert_eq!(bytes.len(), BALANCE_QUERY_LEN);
        assert_eq!(decode_balance_query(&bytes), Some(q));
    }

    #[test]
    fn response_roundtrip_is_exact_length() {
        let r = BalanceResponse {
            address: addr(0x07),
            balance: 11_000_000_000, // 110_000 NIM in luna
            head_height: 4_428_402,
            network_id: 5,
        };
        let bytes = encode_balance_response(&r);
        assert_eq!(bytes.len(), BALANCE_RESPONSE_LEN);
        assert_eq!(decode_balance_response(&bytes), Some(r));
    }

    #[test]
    fn decoders_reject_wrong_length() {
        assert_eq!(decode_balance_query(&[0u8; 19]), None);
        assert_eq!(decode_balance_query(&[0u8; 21]), None);
        assert_eq!(decode_balance_response(&[0u8; 32]), None);
        assert_eq!(decode_balance_response(&[0u8; 34]), None);
        assert_eq!(decode_balance_query(&[]), None);
    }

    #[test]
    fn cache_keeps_only_the_freshest_balance() {
        let mut c = BalanceCache::new();
        let a = addr(1);
        assert!(c.observe(&BalanceResponse {
            address: a,
            balance: 100,
            head_height: 10,
            network_id: 5
        }));
        // A fresher head updates.
        assert!(c.observe(&BalanceResponse {
            address: a,
            balance: 250,
            head_height: 20,
            network_id: 5
        }));
        assert_eq!(c.get(&a).unwrap().balance, 250);
        assert_eq!(c.get(&a).unwrap().head_height, 20);
        // A stale (older or equal head) response is ignored — no rollback.
        assert!(!c.observe(&BalanceResponse {
            address: a,
            balance: 1,
            head_height: 20,
            network_id: 5
        }));
        assert!(!c.observe(&BalanceResponse {
            address: a,
            balance: 1,
            head_height: 5,
            network_id: 5
        }));
        assert_eq!(c.get(&a).unwrap().balance, 250);
    }

    #[test]
    fn cache_rejects_wrong_network() {
        let mut c = BalanceCache::for_network(5);
        // Mainnet (24) response on a testnet-pinned cache is rejected.
        assert!(!c.observe(&BalanceResponse {
            address: addr(2),
            balance: 999,
            head_height: 1,
            network_id: 24
        }));
        assert!(c.is_empty());
        // Same address on the right network is accepted.
        assert!(c.observe(&BalanceResponse {
            address: addr(2),
            balance: 999,
            head_height: 1,
            network_id: 5
        }));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn cache_tracks_multiple_addresses() {
        let mut c = BalanceCache::new();
        c.observe(&BalanceResponse {
            address: addr(1),
            balance: 10,
            head_height: 1,
            network_id: 5,
        });
        c.observe(&BalanceResponse {
            address: addr(2),
            balance: 20,
            head_height: 1,
            network_id: 5,
        });
        assert_eq!(c.len(), 2);
        assert_eq!(c.get(&addr(1)).unwrap().balance, 10);
        assert_eq!(c.get(&addr(2)).unwrap().balance, 20);
        assert_eq!(c.get(&addr(3)), None);
    }
}
