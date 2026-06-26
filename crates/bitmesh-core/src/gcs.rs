//! # gcs — a Golomb-Coded-Set membership filter (G7 store-and-forward)
//!
//! PROTOCOL.md "Store-and-forward = GCS gossip-sync": a node advertises **what it has**
//! as a compact probabilistic set so a peer can unicast back only the packets that node
//! is *missing*. A [`GcsFilter`] is that set — a Bitchat/BIP158-style **Golomb-Coded
//! Set**: every member id is hashed uniformly into `[0, N·M)`, the sorted values are
//! delta-encoded, and each delta is **Golomb-Rice coded** (unary quotient + `P`-bit
//! remainder). It answers membership with **no false negatives** and a tunable
//! false-positive rate of **1/M ≈ 0.01**, in roughly `~8.6` bits per id.
//!
//! ## Why these parameters
//!
//! - `M = 100` → the false-positive rate is `1/M = 0.01` (a random non-member collides
//!   with one of the `N` mapped values with probability `N/(N·M) = 1/M`).
//! - `P = 6` (Rice modulus `2^6 = 64 ≈ 0.69·M`) is the near-optimal Golomb parameter for
//!   the geometric gap distribution, minimising the encoded size.
//! - At `N = 360` ids the encoding is a deterministic `≤ ~388 B` (the summed unary
//!   quotients are bounded by `last_value / 64 ≤ N·M/64`), comfortably under the
//!   [`GCS_MAX_BYTES`] `400 B` budget — see [`GCS_MAX_ITEMS`].
//!
//! ## Hostile input
//!
//! [`GcsFilter::from_bytes`] accepts arbitrary peer bytes and [`GcsFilter::contains`] is
//! **panic-free** over any decoded stream (a truncated / corrupt filter simply yields
//! "not present", so the peer re-sends a packet the node may already have — harmless).
//!
//! **Non-money-path** — this is a relay-layer membership filter over opaque packet ids;
//! nothing here signs, broadcasts, or inspects key material.

/// The target false-positive rate of the filter (`1 / GCS_M`).
pub const GCS_FALSE_POSITIVE_RATE: f64 = 0.01;

/// The hash-range multiplier: each id maps into `[0, N·M)`, giving an `1/M` fp rate.
pub const GCS_M: u64 = 100;

/// The Golomb-Rice parameter: the remainder of each delta is stored in `P` bits and the
/// quotient in unary. `2^6 = 64 ≈ 0.69·M` is near-optimal for the gap distribution.
pub const GCS_P: u32 = 6;

/// The advertised-filter byte budget (PROTOCOL.md: "≤ 400 B").
pub const GCS_MAX_BYTES: usize = 400;

/// The most ids a single advertised filter carries so it stays within [`GCS_MAX_BYTES`].
/// At `~8.6` bits/id, `360` ids encode to `≤ ~388 B` (deterministically bounded).
pub const GCS_MAX_ITEMS: usize = 360;

/// Murmur3 64-bit finalizer — spreads a (possibly structured) id into a uniform 64-bit
/// value so the range mapping is even and the false-positive rate holds.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
    z = (z ^ (z >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    z ^ (z >> 33)
}

/// Map an id uniformly into `[0, range)` (BIP158 `hash_to_range`: a 128-bit multiply-
/// shift, an unbiased reduction). `range == 0` yields `0`.
fn hash_to_range(id: u64, range: u64) -> u64 {
    if range == 0 {
        return 0;
    }
    ((mix64(id) as u128 * range as u128) >> 64) as u64
}

/// A minimal MSB-first bit writer backing the Golomb-Rice stream.
struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            bytes: Vec::new(),
            nbits: 0,
        }
    }

    fn write_bit(&mut self, bit: bool) {
        let byte_idx = self.nbits / 8;
        if byte_idx == self.bytes.len() {
            self.bytes.push(0);
        }
        if bit {
            let bit_idx = 7 - (self.nbits % 8);
            self.bytes[byte_idx] |= 1 << bit_idx;
        }
        self.nbits += 1;
    }

    fn write_bits(&mut self, val: u64, n: u32) {
        for i in (0..n).rev() {
            self.write_bit((val >> i) & 1 == 1);
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A minimal MSB-first bit reader. Returns `None` at end-of-input (never panics).
struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        BitReader { bytes, pos: 0 }
    }

    fn read_bit(&mut self) -> Option<bool> {
        let byte_idx = self.pos / 8;
        if byte_idx >= self.bytes.len() {
            return None;
        }
        let bit_idx = 7 - (self.pos % 8);
        let bit = (self.bytes[byte_idx] >> bit_idx) & 1 == 1;
        self.pos += 1;
        Some(bit)
    }

    fn read_bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u64;
        }
        Some(v)
    }
}

/// Golomb-Rice encode `v` with parameter `p`: `v >> p` ones, a `0` terminator, then the
/// low `p` bits of `v`.
fn rice_encode(w: &mut BitWriter, v: u64, p: u32) {
    let q = v >> p;
    for _ in 0..q {
        w.write_bit(true);
    }
    w.write_bit(false);
    w.write_bits(v & ((1u64 << p) - 1), p);
}

/// Golomb-Rice decode one value with parameter `p`. `None` if the stream runs out
/// mid-symbol (truncated / corrupt input).
fn rice_decode(r: &mut BitReader, p: u32) -> Option<u64> {
    let mut q = 0u64;
    while r.read_bit()? {
        q += 1;
    }
    let rem = r.read_bits(p)?;
    Some((q << p) | rem)
}

/// A Golomb-Coded-Set membership filter over a set of `u64` ids.
///
/// Built with [`GcsFilter::build`] from the ids a node holds; queried with
/// [`GcsFilter::contains`]; serialized to/from the wire with [`GcsFilter::to_bytes`] /
/// [`GcsFilter::from_bytes`] for the `requestSync` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcsFilter {
    /// The number of ids the filter was built from (sets the hash range `n·M`).
    n: u32,
    /// The Golomb-Rice coded stream of sorted, delta-encoded mapped values.
    data: Vec<u8>,
}

impl GcsFilter {
    /// Build a filter advertising membership of every id in `ids`.
    ///
    /// Ids are mapped uniformly into `[0, n·M)`, sorted, and Golomb-Rice delta-coded.
    /// Duplicate ids (or hash collisions) collapse to a zero delta and are harmless.
    pub fn build(ids: &[u64]) -> Self {
        let n = ids.len();
        if n == 0 {
            return GcsFilter {
                n: 0,
                data: Vec::new(),
            };
        }
        let range = n as u64 * GCS_M;
        let mut mapped: Vec<u64> = ids.iter().map(|&id| hash_to_range(id, range)).collect();
        mapped.sort_unstable();

        let mut w = BitWriter::new();
        let mut last = 0u64;
        for v in mapped {
            rice_encode(&mut w, v - last, GCS_P);
            last = v;
        }
        GcsFilter {
            n: n as u32,
            data: w.into_bytes(),
        }
    }

    /// Whether `id` is (probably) a member. **No false negatives**: a real member always
    /// returns `true`. A non-member returns `true` with probability `~1/M ≈ 0.01`.
    pub fn contains(&self, id: u64) -> bool {
        if self.n == 0 {
            return false;
        }
        let range = self.n as u64 * GCS_M;
        let target = hash_to_range(id, range);
        let mut r = BitReader::new(&self.data);
        let mut acc = 0u64;
        for _ in 0..self.n {
            let Some(delta) = rice_decode(&mut r, GCS_P) else {
                return false;
            };
            acc = acc.saturating_add(delta);
            if acc == target {
                return true;
            }
            if acc > target {
                return false; // the sorted stream passed the target — absent.
            }
        }
        false
    }

    /// The number of ids the filter was built from.
    pub fn len(&self) -> usize {
        self.n as usize
    }

    /// Whether the filter carries no ids (membership is always `false`).
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The serialized size in bytes (`2`-byte count + the coded stream).
    pub fn byte_len(&self) -> usize {
        2 + self.data.len()
    }

    /// Serialize for the `requestSync` payload: `n(2, BE) | rice-coded stream`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        out.extend_from_slice(&(self.n as u16).to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parse a filter from peer bytes. `None` only if the 2-byte count header is missing;
    /// a truncated stream still parses (queries against it just degrade to "absent").
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 2 {
            return None;
        }
        let n = u16::from_be_bytes([bytes[0], bytes[1]]) as u32;
        Some(GcsFilter {
            n,
            data: bytes[2..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{RelayRng, XorShiftRng};

    /// Distinct pseudo-random ids from a seeded PRNG (reproducible).
    fn ids(seed: u64, count: usize) -> Vec<u64> {
        let mut rng = XorShiftRng::seeded(seed);
        (0..count).map(|_| rng.next_u64()).collect()
    }

    #[test]
    fn membership_has_no_false_negatives() {
        for &count in &[1usize, 5, 50, 200, 500] {
            let members = ids(0xA11CE, count);
            let f = GcsFilter::build(&members);
            assert_eq!(f.len(), count);
            for &m in &members {
                assert!(f.contains(m), "member {m} reported absent (false negative)");
            }
        }
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let f = GcsFilter::build(&[]);
        assert!(f.is_empty());
        assert!(!f.contains(0));
        assert!(!f.contains(123_456_789));
    }

    #[test]
    fn wire_round_trip_preserves_membership() {
        let members = ids(0xBEEF, 120);
        let f = GcsFilter::build(&members);
        let back = GcsFilter::from_bytes(&f.to_bytes()).expect("our own bytes parse");
        assert_eq!(back, f);
        for &m in &members {
            assert!(back.contains(m));
        }
    }

    #[test]
    fn empirical_false_positive_rate_is_near_target() {
        // Build from one disjoint id stream, query 100k ids from another. With uniform
        // hashing the hit rate should sit at ~1/M = 0.01.
        let members = ids(0x5EED_0001, 500);
        let f = GcsFilter::build(&members);

        let queries = ids(0x5EED_0002, 100_000);
        let mut hits = 0usize;
        for q in queries {
            if f.contains(q) {
                hits += 1;
            }
        }
        let rate = hits as f64 / 100_000.0;
        assert!(
            (0.005..=0.02).contains(&rate),
            "empirical fp rate {rate} is not near {GCS_FALSE_POSITIVE_RATE}"
        );
    }

    #[test]
    fn max_items_filter_fits_the_byte_budget() {
        let members = ids(0xF17, GCS_MAX_ITEMS);
        let f = GcsFilter::build(&members);
        assert!(
            f.byte_len() <= GCS_MAX_BYTES,
            "filter of {GCS_MAX_ITEMS} ids is {} B (> {GCS_MAX_BYTES} B budget)",
            f.byte_len()
        );
    }

    #[test]
    fn corrupt_filter_never_panics() {
        // Arbitrary bytes claiming a large item count over a tiny stream.
        let f = GcsFilter::from_bytes(&[0xFF, 0xFF, 0x01, 0x02, 0x03]).unwrap();
        let _ = f.contains(42); // must terminate, must not panic.
        assert!(GcsFilter::from_bytes(&[0x00]).is_none());
    }
}
