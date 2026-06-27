//! # ratelimit — G12 per-peer inbound rate limiting (anti-DoS)
//!
//! A flood-routed mesh is only as healthy as its willingness to *stop* carrying a peer that
//! is abusing it. This is a per-peer **token bucket**: each connected peer gets a generous
//! steady rate plus a burst allowance; once a peer exceeds it, its further inbound frames are
//! dropped before they cost any decode / relay airtime (RISKS.md #4, "per-peer relay rate
//! limits"). A well-behaved peer never notices; a flooding one is throttled to the bucket.
//!
//! Clock-free by construction — the caller passes the worker's monotonic `now_ms`, so the
//! whole thing is deterministic under test (no wall clock, no real sleeps). The tracked-peer
//! map is bounded so peer-id churn can't grow memory without limit.

use std::collections::HashMap;

/// Steady-state inbound budget per peer, in frames per second.
pub const PEER_REFILL_PER_SEC: u32 = 64;
/// Burst allowance per peer (bucket capacity) — absorbs legitimate bursts, caps a flood.
pub const PEER_BUCKET_CAPACITY: u32 = 256;
/// Max peers tracked at once (BLE degree is small; this only bounds peer-id churn).
const MAX_TRACKED_PEERS: usize = 128;

/// A single peer's token bucket.
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

/// Per-peer token-bucket rate limiter for inbound frames.
pub struct PeerRateLimiter {
    capacity: f64,
    refill_per_ms: f64,
    buckets: HashMap<String, Bucket>,
}

impl Default for PeerRateLimiter {
    fn default() -> Self {
        Self::with_rate(PEER_BUCKET_CAPACITY, PEER_REFILL_PER_SEC)
    }
}

impl PeerRateLimiter {
    /// A limiter with the default budget ([`PEER_BUCKET_CAPACITY`] / [`PEER_REFILL_PER_SEC`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// A limiter with a caller-chosen capacity + refill (used by tests to force the edges).
    pub fn with_rate(capacity: u32, refill_per_sec: u32) -> Self {
        PeerRateLimiter {
            capacity: capacity as f64,
            refill_per_ms: refill_per_sec as f64 / 1000.0,
            buckets: HashMap::new(),
        }
    }

    /// Consume one token for `peer` at `now_ms`; return `true` if the frame is within the
    /// peer's rate (process it) or `false` if the peer is flooding (drop it). A peer seen for
    /// the first time starts with a full bucket.
    pub fn allow(&mut self, peer: &str, now_ms: u64) -> bool {
        self.evict_if_full(peer, now_ms);
        let capacity = self.capacity;
        let refill_per_ms = self.refill_per_ms;
        let bucket = self.buckets.entry(peer.to_string()).or_insert(Bucket {
            tokens: capacity,
            last_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64;
        bucket.tokens = (bucket.tokens + elapsed * refill_per_ms).min(capacity);
        bucket.last_ms = now_ms;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Bound the map: if it is full and `peer` is new, drop the least-recently-seen bucket.
    fn evict_if_full(&mut self, peer: &str, _now_ms: u64) {
        if self.buckets.len() < MAX_TRACKED_PEERS || self.buckets.contains_key(peer) {
            return;
        }
        if let Some(oldest) = self
            .buckets
            .iter()
            .min_by_key(|(_, b)| b.last_ms)
            .map(|(k, _)| k.clone())
        {
            self.buckets.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_within_capacity_is_allowed_then_throttled() {
        let mut rl = PeerRateLimiter::with_rate(4, 1); // tiny bucket, slow refill
                                                       // 4 frames at the same instant drain the full bucket …
        for i in 0..4 {
            assert!(rl.allow("peer", 0), "frame {i} should be allowed");
        }
        // … the 5th in the same instant is throttled.
        assert!(!rl.allow("peer", 0));
    }

    #[test]
    fn tokens_refill_over_time() {
        let mut rl = PeerRateLimiter::with_rate(2, 10); // 10/sec → 1 token per 100 ms
        assert!(rl.allow("peer", 0));
        assert!(rl.allow("peer", 0));
        assert!(!rl.allow("peer", 0)); // drained
                                       // After 100 ms one token has refilled.
        assert!(rl.allow("peer", 100));
        assert!(!rl.allow("peer", 100));
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let mut rl = PeerRateLimiter::with_rate(3, 1000);
        // A long idle gap can't bank more than the capacity.
        assert!(rl.allow("peer", 1_000_000));
        assert!(rl.allow("peer", 1_000_000));
        assert!(rl.allow("peer", 1_000_000));
        assert!(!rl.allow("peer", 1_000_000));
    }

    #[test]
    fn peers_are_independent() {
        let mut rl = PeerRateLimiter::with_rate(1, 1);
        assert!(rl.allow("a", 0));
        assert!(!rl.allow("a", 0)); // a is drained …
        assert!(rl.allow("b", 0)); // … but b has its own bucket.
    }

    #[test]
    fn tracked_peers_are_bounded() {
        let mut rl = PeerRateLimiter::with_rate(8, 1);
        for i in 0..(MAX_TRACKED_PEERS + 50) {
            assert!(rl.allow(&format!("peer-{i}"), i as u64));
        }
        assert!(rl.buckets.len() <= MAX_TRACKED_PEERS);
    }
}
