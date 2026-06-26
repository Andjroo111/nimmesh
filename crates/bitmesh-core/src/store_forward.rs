//! # store_forward — GCS gossip-sync recent-packet cache + sync scheduling (G7)
//!
//! PROTOCOL.md "Store-and-forward = GCS gossip-sync (the key offline-origination piece)":
//! every node keeps the packets it has recently seen in a typed cache and periodically
//! advertises that set as a [`crate::gcs::GcsFilter`] inside a `requestSync` (`0x21`,
//! ttl 0, local-only). A peer unicasts back anything **not** in the filter, flagged
//! `isRSR`. An originator or gateway that was **out of range** rejoining within the
//! **15-minute** retention window thereby auto-catches-up on the packets it missed —
//! comfortably inside Nimiq's ~2 h transaction-validity budget.
//!
//! This module owns the two clock-free building blocks the engine wires together:
//!
//! - [`RecentCache`] — a bounded recent-packet store (≤ [`RECENT_CACHE_CAP`] entries,
//!   [`RECENT_RETENTION_MS`] age horizon, oldest evicted) keyed by a header-derived
//!   [`packet_id`]. Like the G6 reassembler it is **clock-free**: the caller passes a
//!   monotonic `now_ms` on every mutation, so tests are fully deterministic.
//! - [`SyncScheduler`] — a rate-limiter that fires at most once per [`SYNC_TICK_MS`]
//!   (the 30 s maintenance tick), also clock-free.
//!
//! **Non-money-path** — the cache holds opaque [`Packet`]s and serves them verbatim;
//! nothing here signs, broadcasts, or inspects key material.

use std::collections::{HashMap, VecDeque};

use crate::gcs::{GcsFilter, GCS_MAX_ITEMS};
use crate::packet::Packet;

/// Maximum recent packets a node caches (oldest evicted past this — hostile-flood
/// ceiling, RISKS.md #4).
pub const RECENT_CACHE_CAP: usize = 1000;

/// The catch-up horizon: packets older than this (15 min) are dropped. Equal to Bitchat's
/// `maxMessageAgeSeconds = 900`, comfortably inside Nimiq's ~2 h validity window.
pub const RECENT_RETENTION_MS: u64 = 900_000;

/// The "active"/hot recency window: a packet inserted within the last 15 s is considered
/// freshly active (observability + cadence reasoning).
pub const SYNC_ACTIVE_MS: u64 = 15_000;

/// The maintenance cadence: a node issues a `requestSync` at most once per 30 s.
pub const SYNC_TICK_MS: u64 = 30_000;

/// A recent-packet identity: a 64-bit hash of the packet **header** (`type`, `senderID`,
/// `timestamp`), matching the blind relay-dedup identity so the same logical packet has a
/// stable id across hops (TTL is intentionally excluded).
pub type PacketId = u64;

/// Derive a [`PacketId`] from a packet's header fields (FNV-1a over `type | sender |
/// timestamp`). Payload and TTL are excluded so a relayed copy keeps the same id.
pub fn packet_id(p: &Packet) -> PacketId {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    };
    mix(p.msg_type.to_u8());
    for b in p.sender_id {
        mix(b);
    }
    for b in p.timestamp_ms.to_be_bytes() {
        mix(b);
    }
    h
}

/// A bounded, clock-free store of the packets a node has recently seen.
///
/// Insertion-ordered for both LRU eviction (at [`RECENT_CACHE_CAP`]) and age GC (past
/// [`RECENT_RETENTION_MS`]). The caller supplies a monotonic `now_ms` on every mutating
/// call, keeping the cache a pure value that is trivially deterministic under test.
#[derive(Default)]
pub struct RecentCache {
    /// id → (the stored packet, its insertion time in ms).
    map: HashMap<PacketId, (Packet, u64)>,
    /// Insertion order (front = oldest) for eviction + age GC.
    order: VecDeque<PacketId>,
}

impl RecentCache {
    /// A fresh, empty cache.
    pub fn new() -> Self {
        RecentCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Remember `packet` under `id` at logical time `now_ms`. Returns `true` if it was
    /// newly stored, `false` if already present (a duplicate). Expired entries are GC'd
    /// first; at capacity the oldest entry is evicted.
    pub fn insert(&mut self, id: PacketId, packet: Packet, now_ms: u64) -> bool {
        self.gc(now_ms);
        if self.map.contains_key(&id) {
            return false;
        }
        if self.map.len() >= RECENT_CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(id, (packet, now_ms));
        self.order.push_back(id);
        true
    }

    /// Whether `id` is currently cached.
    pub fn contains(&self, id: PacketId) -> bool {
        self.map.contains_key(&id)
    }

    /// The number of packets currently cached.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// How many cached packets fall inside the [`SYNC_ACTIVE_MS`] hot window at `now_ms`.
    pub fn active_len(&self, now_ms: u64) -> usize {
        self.map
            .values()
            .filter(|(_, t)| now_ms.saturating_sub(*t) <= SYNC_ACTIVE_MS)
            .count()
    }

    /// The ids to advertise in a `requestSync` filter: the most-recent up to
    /// [`GCS_MAX_ITEMS`] live entries (so the filter stays within the byte budget).
    pub fn advertise_ids(&mut self, now_ms: u64) -> Vec<PacketId> {
        self.gc(now_ms);
        self.order
            .iter()
            .rev()
            .take(GCS_MAX_ITEMS)
            .copied()
            .collect()
    }

    /// Build the [`GcsFilter`] advertising what this node holds (the `requestSync` body).
    pub fn build_filter(&mut self, now_ms: u64) -> GcsFilter {
        let ids = self.advertise_ids(now_ms);
        GcsFilter::build(&ids)
    }

    /// The cached packets a requester is **missing** — every live entry whose id is not in
    /// `filter` — cloned so the caller can re-encode + unicast them as `isRSR` replies.
    pub fn missing(&mut self, filter: &GcsFilter, now_ms: u64) -> Vec<Packet> {
        self.gc(now_ms);
        self.order
            .iter()
            .filter(|id| !filter.contains(**id))
            .filter_map(|id| self.map.get(id).map(|(p, _)| p.clone()))
            .collect()
    }

    /// Drop entries older than [`RECENT_RETENTION_MS`] (front of the order is oldest).
    fn gc(&mut self, now_ms: u64) {
        while let Some(&front) = self.order.front() {
            match self.map.get(&front) {
                Some((_, t)) if now_ms.saturating_sub(*t) > RECENT_RETENTION_MS => {
                    self.order.pop_front();
                    self.map.remove(&front);
                }
                Some(_) => break,
                None => {
                    self.order.pop_front();
                }
            }
        }
    }
}

/// A clock-free rate-limiter for the periodic `requestSync` maintenance tick.
///
/// [`due`](SyncScheduler::due) returns `true` at most once per [`SYNC_TICK_MS`]; the
/// caller passes a monotonic `now_ms`, so the cadence is deterministic under test.
#[derive(Default)]
pub struct SyncScheduler {
    last_ms: Option<u64>,
}

impl SyncScheduler {
    /// A fresh scheduler that is due on its first poll.
    pub fn new() -> Self {
        SyncScheduler { last_ms: None }
    }

    /// Whether a sync is due at `now_ms`. The first poll is always due; afterwards it
    /// fires only once every [`SYNC_TICK_MS`]. Records the firing time when it returns
    /// `true`.
    pub fn due(&mut self, now_ms: u64) -> bool {
        match self.last_ms {
            Some(t) if now_ms.saturating_sub(t) < SYNC_TICK_MS => false,
            _ => {
                self.last_ms = Some(now_ms);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{MessageType, PEER_ID_LEN};

    fn pkt(sender: u8, ts: u64) -> Packet {
        let mut p = Packet::new(MessageType::NimiqTx, [sender; PEER_ID_LEN], vec![sender]);
        p.timestamp_ms = ts;
        p
    }

    #[test]
    fn packet_id_is_header_only_and_stable_across_ttl() {
        let mut a = pkt(1, 42);
        let b = a.clone();
        assert_eq!(packet_id(&a), packet_id(&b));
        // TTL + payload do not change the id (relayed copies share an identity).
        a.ttl = 3;
        a.payload = vec![9, 9, 9];
        assert_eq!(packet_id(&a), packet_id(&b));
        // A different timestamp does.
        let c = pkt(1, 43);
        assert_ne!(packet_id(&a), packet_id(&c));
    }

    #[test]
    fn insert_dedups_and_reports_first_sight() {
        let mut c = RecentCache::new();
        let p = pkt(1, 1);
        let id = packet_id(&p);
        assert!(c.insert(id, p.clone(), 0));
        assert!(!c.insert(id, p, 0));
        assert_eq!(c.len(), 1);
        assert!(c.contains(id));
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut c = RecentCache::new();
        for i in 0..(RECENT_CACHE_CAP + 10) {
            let p = pkt(1, i as u64);
            c.insert(packet_id(&p), p, i as u64);
        }
        assert_eq!(c.len(), RECENT_CACHE_CAP);
        // The very first packets were evicted.
        let first = pkt(1, 0);
        assert!(!c.contains(packet_id(&first)));
    }

    #[test]
    fn expired_entries_are_gced() {
        let mut c = RecentCache::new();
        let old = pkt(1, 1);
        c.insert(packet_id(&old), old.clone(), 0);
        assert_eq!(c.len(), 1);
        // A much-later insert GCs anything past the retention horizon.
        let fresh = pkt(2, 2);
        c.insert(packet_id(&fresh), fresh, RECENT_RETENTION_MS + 1);
        assert!(!c.contains(packet_id(&old)));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn active_window_counts_only_recent_entries() {
        let mut c = RecentCache::new();
        let a = pkt(1, 1);
        c.insert(packet_id(&a), a, 0);
        let b = pkt(2, 2);
        c.insert(packet_id(&b), b, 1000);
        // At t = 2000 both are within the 15 s active window …
        assert_eq!(c.active_len(2000), 2);
        // … but well past it, none are.
        assert_eq!(c.active_len(SYNC_ACTIVE_MS + 5000), 0);
    }

    #[test]
    fn build_filter_and_missing_round_trip() {
        // A holds five packets; B holds the first three. B's filter should make A report
        // exactly the two B is missing.
        let mut a = RecentCache::new();
        let mut packets = Vec::new();
        for i in 0..5u64 {
            let p = pkt(1, i + 1);
            packets.push(p.clone());
            a.insert(packet_id(&p), p, i);
        }
        let mut b = RecentCache::new();
        for p in packets.iter().take(3) {
            b.insert(packet_id(p), p.clone(), 0);
        }
        let b_filter = b.build_filter(0);

        let missing = a.missing(&b_filter, 10);
        assert_eq!(missing.len(), 2);
        let missing_ts: Vec<u64> = missing.iter().map(|p| p.timestamp_ms).collect();
        assert!(missing_ts.contains(&4) && missing_ts.contains(&5));
    }

    #[test]
    fn full_filter_leaves_nothing_missing() {
        let mut a = RecentCache::new();
        for i in 0..8u64 {
            let p = pkt(1, i + 1);
            a.insert(packet_id(&p), p, i);
        }
        let filter = a.build_filter(0);
        // A peer that already has everything A advertises is missing nothing.
        assert!(a.missing(&filter, 10).is_empty());
    }

    #[test]
    fn scheduler_fires_once_per_tick() {
        let mut s = SyncScheduler::new();
        assert!(s.due(0)); // first poll fires.
        assert!(!s.due(1_000)); // too soon.
        assert!(!s.due(SYNC_TICK_MS - 1));
        assert!(s.due(SYNC_TICK_MS)); // a full tick later — fires again.
        assert!(!s.due(SYNC_TICK_MS + 1));
    }
}
