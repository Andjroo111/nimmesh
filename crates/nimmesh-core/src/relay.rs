//! # relay — G6 relay sophistication (degree-adaptive flooding + jitter + TTL)
//!
//! G5 shipped a *basic* relay in [`crate::engine`]: blind LRU dedup → TTL-decrement →
//! re-flood. This module adds the PROTOCOL.md "TTL / hop cap & relay" refinements that
//! keep a dense Bluetooth mesh from melting down, while staying **fully deterministic
//! under test** (no real sleeps, no wall-clock randomness):
//!
//! - **Degree-adaptive probabilistic relay.** In a sparse mesh (peer-degree below
//!   [`HIGH_DEGREE_THRESHOLD`]) every flooded packet is *always* relayed — reachability
//!   wins. In a dense mesh (degree at/above the threshold) each packet is relayed only
//!   with [`RELAY_PROBABILITY`], because a neighbour almost certainly already heard it
//!   from someone else. This is the gossip-flooding "fanout" damping that stops a
//!   broadcast storm.
//! - **Relay jitter.** A small random delay in `[JITTER_MIN_MS, JITTER_MAX_MS]` before a
//!   rebroadcast de-synchronises neighbours so their BLE writes don't collide. The delay
//!   *source* is an injectable [`RelayDelay`] trait — real [`RealDelay`] in production,
//!   zero-cost [`NoDelay`] in tests — so the suite never actually sleeps.
//! - **TTL hop cap.** [`relayed_ttl`] caps an inbound TTL at [`DEFAULT_TTL`] then
//!   decrements, dropping at the floor. Capping a hostile over-large TTL is what makes
//!   the mesh **loop-free**: every relay strictly shrinks the budget, so a packet can
//!   never circulate forever (property-tested).
//!
//! Both the relay decision and the jitter draw from an injectable, seeded [`RelayRng`],
//! so a test fixes the seed and gets identical decisions every run. **Non-money-path:**
//! nothing here reads, signs, or broadcasts a tx — it only decides *whether/when* an
//! already-opaque packet is rebroadcast.

use std::time::Duration;

use crate::packet::DEFAULT_TTL;

/// Peer-degree at/above which a node switches from "always relay" to probabilistic
/// relay (PROTOCOL.md: "high-degree threshold 6").
pub const HIGH_DEGREE_THRESHOLD: usize = 6;

/// Probability a high-degree node rebroadcasts a given flooded packet.
pub const RELAY_PROBABILITY: f64 = 0.5;

/// Minimum relay jitter before a rebroadcast (PROTOCOL.md: "relay jitter 10–220 ms").
pub const JITTER_MIN_MS: u64 = 10;
/// Maximum relay jitter before a rebroadcast.
pub const JITTER_MAX_MS: u64 = 220;

/// The hop-cap transform a relay applies to a packet's TTL.
///
/// Mirrors PROTOCOL.md exactly: `ttlLimit = min(ttl, 7)`, `newTTL = ttlLimit - 1`, and
/// the packet is **dropped** (returns `None`) when the capped budget is `<= 1`. Capping
/// first means even a hostile `ttl = 255` collapses to a 7-hop budget — the invariant
/// that makes the flood loop-free.
pub fn relayed_ttl(ttl: u8) -> Option<u8> {
    let capped = if ttl < DEFAULT_TTL { ttl } else { DEFAULT_TTL };
    if capped <= 1 {
        None
    } else {
        Some(capped - 1)
    }
}

/// An injectable random source for relay decisions (seeded → deterministic in tests).
///
/// Only the relay layer's coarse decisions ride this — never key material. The default
/// methods turn raw `u64` draws into a Bernoulli trial and a bounded delay so an impl
/// only has to supply `next_u64`.
pub trait RelayRng: Send {
    /// The next pseudo-random `u64`.
    fn next_u64(&mut self) -> u64;

    /// Return `true` with probability `p` (clamped to `[0, 1]`).
    fn bernoulli(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }

    /// A value uniformly in `[min, max]` (inclusive); `min` if the range is degenerate.
    fn in_range_ms(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        let span = max - min + 1;
        min + (self.next_u64() % span)
    }
}

/// A tiny, fast xorshift64 PRNG. Deterministic for a given seed — adequate for relay
/// fanout / jitter decisions (it is **not** cryptographic and never touches money).
pub struct XorShiftRng {
    state: u64,
}

impl XorShiftRng {
    /// A generator seeded with `seed` (a zero seed is nudged to a fixed non-zero state,
    /// since xorshift is stuck at zero).
    pub fn seeded(seed: u64) -> Self {
        XorShiftRng {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }
}

impl RelayRng for XorShiftRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

/// An injectable delay source for relay jitter.
///
/// Production sleeps the worker thread ([`RealDelay`]); tests use [`NoDelay`] so the
/// suite is instant and deterministic (PROTOCOL.md jitter is purely collision-avoidance;
/// zero jitter is functionally correct, just less collision-resistant).
pub trait RelayDelay: Send {
    /// Block the calling (worker) thread for `d`.
    fn sleep(&self, d: Duration);
}

/// The production delay: actually sleeps the worker thread.
pub struct RealDelay;
impl RelayDelay for RealDelay {
    fn sleep(&self, d: Duration) {
        if d > Duration::ZERO {
            std::thread::sleep(d);
        }
    }
}

/// The test/headless delay: returns immediately (no real sleep).
pub struct NoDelay;
impl RelayDelay for NoDelay {
    fn sleep(&self, _d: Duration) {}
}

/// The bundled relay policy a worker thread holds: an [`RelayRng`] + a [`RelayDelay`]
/// plus the degree threshold, relay probability, and jitter bounds.
///
/// Lives **worker-thread-local** in [`crate::engine::WorkerState`] (no locks on the hot
/// path). `production()` floods with real jitter and a time-seeded RNG; `deterministic()`
/// is the zero-sleep, fixed-seed policy the harness and unit tests use.
pub struct RelayPolicy {
    rng: Box<dyn RelayRng>,
    delay: Box<dyn RelayDelay>,
    threshold: usize,
    probability: f64,
    jitter_min_ms: u64,
    jitter_max_ms: u64,
}

impl RelayPolicy {
    /// The real-device policy: time-seeded RNG + real jitter sleeps.
    pub fn production() -> Self {
        RelayPolicy::custom(
            Box::new(XorShiftRng::seeded(seed_from_clock())),
            Box::new(RealDelay),
        )
    }

    /// The headless/test policy: fixed seed + zero-cost (no real sleep) delay.
    pub fn deterministic() -> Self {
        RelayPolicy::custom(
            Box::new(XorShiftRng::seeded(0xD1CE_5EED)),
            Box::new(NoDelay),
        )
    }

    /// A policy with a caller-supplied RNG + delay (default threshold/probability/jitter).
    pub fn custom(rng: Box<dyn RelayRng>, delay: Box<dyn RelayDelay>) -> Self {
        RelayPolicy {
            rng,
            delay,
            threshold: HIGH_DEGREE_THRESHOLD,
            probability: RELAY_PROBABILITY,
            jitter_min_ms: JITTER_MIN_MS,
            jitter_max_ms: JITTER_MAX_MS,
        }
    }

    /// Decide whether to rebroadcast a flooded packet given this node's peer `degree`.
    ///
    /// Below [`HIGH_DEGREE_THRESHOLD`] → always relay (sparse mesh, reachability first).
    /// At/above it → relay only with [`RELAY_PROBABILITY`] (dense mesh fanout damping).
    pub fn should_relay(&mut self, degree: usize) -> bool {
        self.should_relay_throttled(degree, 1.0)
    }

    /// Like [`should_relay`](Self::should_relay) but scales the relay probability by a
    /// battery-aware `factor` in `[0, 1]` (G20). `factor = 1.0` reproduces `should_relay`
    /// exactly — a sparse-mesh draw of `bernoulli(1.0)` short-circuits to `true` without
    /// touching the RNG, so existing (deterministic) behaviour is unchanged. A lower factor
    /// damps the fanout (a low-battery "good citizen" relays less); `0.0` never relays.
    pub fn should_relay_throttled(&mut self, degree: usize, factor: f64) -> bool {
        let base = if degree < self.threshold {
            1.0
        } else {
            self.probability
        };
        self.rng.bernoulli((base * factor).clamp(0.0, 1.0))
    }

    /// Draw the next relay jitter delay (does not sleep).
    pub fn next_jitter(&mut self) -> Duration {
        Duration::from_millis(self.rng.in_range_ms(self.jitter_min_ms, self.jitter_max_ms))
    }

    /// Draw a jitter delay and apply it via the injected [`RelayDelay`] (a no-op under
    /// [`NoDelay`], so tests never actually sleep).
    pub fn apply_jitter(&mut self) {
        let d = self.next_jitter();
        self.delay.sleep(d);
    }
}

/// A best-effort, non-cryptographic seed for the production RNG, mixed from the wall
/// clock so two co-located nodes don't relay in lockstep.
pub(crate) fn seed_from_clock() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5DEE_CE66_D000_0001)
        | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A degenerate RNG that always returns a fixed value — used to force the Bernoulli
    /// trial to a known side in tests.
    struct ConstRng(u64);
    impl RelayRng for ConstRng {
        fn next_u64(&mut self) -> u64 {
            self.0
        }
    }

    #[test]
    fn relayed_ttl_caps_at_default_then_decrements() {
        // Hostile over-large TTL collapses to the 7-hop cap, then steps down.
        assert_eq!(relayed_ttl(255), Some(DEFAULT_TTL - 1));
        assert_eq!(relayed_ttl(DEFAULT_TTL), Some(DEFAULT_TTL - 1));
        assert_eq!(relayed_ttl(3), Some(2));
        // Floor: a 1- or 0-budget packet is dropped.
        assert_eq!(relayed_ttl(1), None);
        assert_eq!(relayed_ttl(0), None);
    }

    #[test]
    fn relayed_ttl_strictly_terminates() {
        // From any start, repeated relay reaches None in a bounded number of steps and
        // never increases — the loop-freedom invariant.
        for start in 0u8..=255 {
            let mut ttl = start;
            let mut steps = 0u32;
            while let Some(next) = relayed_ttl(ttl) {
                assert!(next < ttl, "ttl must strictly decrease");
                ttl = next;
                steps += 1;
                assert!(
                    steps <= DEFAULT_TTL as u32,
                    "relay must terminate within the cap"
                );
            }
        }
    }

    #[test]
    fn sparse_mesh_always_relays() {
        let mut p = RelayPolicy::deterministic();
        for degree in 0..HIGH_DEGREE_THRESHOLD {
            assert!(
                p.should_relay(degree),
                "degree {degree} should always relay"
            );
        }
    }

    #[test]
    fn dense_mesh_relay_is_probabilistic() {
        // A draw of 0 is below p=0.5 → relay; a draw of u64::MAX is above → drop. Both
        // only reachable at/above the high-degree threshold.
        let mut relays = RelayPolicy::custom(Box::new(ConstRng(0)), Box::new(NoDelay));
        assert!(relays.should_relay(HIGH_DEGREE_THRESHOLD));

        let mut drops = RelayPolicy::custom(Box::new(ConstRng(u64::MAX)), Box::new(NoDelay));
        assert!(!drops.should_relay(HIGH_DEGREE_THRESHOLD));
        // …but even the "always drop" RNG still relays in a sparse mesh.
        assert!(drops.should_relay(HIGH_DEGREE_THRESHOLD - 1));
    }

    #[test]
    fn throttle_factor_scales_the_relay_decision() {
        // factor 0.0 → never relay, even in a sparse mesh (a critical-battery node).
        let mut off = RelayPolicy::custom(Box::new(ConstRng(0)), Box::new(NoDelay));
        assert!(!off.should_relay_throttled(0, 0.0));
        assert!(!off.should_relay_throttled(HIGH_DEGREE_THRESHOLD, 0.0));

        // factor 1.0 reproduces should_relay exactly: sparse always relays without an RNG draw.
        let mut full = RelayPolicy::custom(Box::new(ConstRng(u64::MAX)), Box::new(NoDelay));
        assert!(full.should_relay_throttled(0, 1.0));

        // A reduced factor in a sparse mesh becomes a real probability roll: a draw of 0 is
        // below 1.0 * 0.5 → relay; a max draw is above → drop.
        let mut relays = RelayPolicy::custom(Box::new(ConstRng(0)), Box::new(NoDelay));
        assert!(relays.should_relay_throttled(0, 0.5));
        let mut drops = RelayPolicy::custom(Box::new(ConstRng(u64::MAX)), Box::new(NoDelay));
        assert!(!drops.should_relay_throttled(0, 0.5));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let mut p = RelayPolicy::deterministic();
        for _ in 0..1000 {
            let ms = p.next_jitter().as_millis() as u64;
            assert!(
                (JITTER_MIN_MS..=JITTER_MAX_MS).contains(&ms),
                "jitter {ms} out of range"
            );
        }
    }

    #[test]
    fn xorshift_is_deterministic_for_a_seed() {
        let mut a = XorShiftRng::seeded(42);
        let mut b = XorShiftRng::seeded(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn no_delay_does_not_block() {
        use std::time::Instant;
        let start = Instant::now();
        NoDelay.sleep(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
