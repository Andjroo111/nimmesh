//! # fragment — G6 fragmentation + bounded reassembly (`fragment = 0x20`)
//!
//! PROTOCOL.md reserves a `fragment` packet (`0x20`) for payloads larger than a single
//! BLE write ([`BLE_DEFAULT_FRAGMENT_SIZE`] ≈ 469 B). Our real ~205-B `nimiqTx` always
//! fits in one packet, so this path **never triggers for today's traffic** — but the
//! protocol defines it for larger / future payloads (e.g. an encrypted memo, a batched
//! sync reply), so the core implements and tests it now.
//!
//! ## Fragment payload layout (rides inside a `fragment` packet's opaque `payload`)
//!
//! ```text
//! fragmentID(8) | index(2, BE) | total(2, BE) | originalType(1) | chunk(...)
//! ```
//!
//! A message of `originalType` and arbitrary length is split into `total` chunks sharing
//! one random `fragmentID`; each chunk is carried in its own `fragment` packet and
//! flooded like any other. A receiver feeds inbound fragments to a [`Reassembler`] which,
//! once every index for a `fragmentID` has arrived, returns the original `(type, bytes)`.
//!
//! ## Bounds (hostile-input discipline, RISKS.md #4)
//!
//! The reassembler holds at most [`MAX_IN_FLIGHT`] partial messages (oldest evicted) and
//! drops any partial older than [`REASSEMBLY_LIFETIME_MS`], so a flood of never-completing
//! fragment sets can't grow memory without bound. The caller supplies a monotonic
//! `now_ms` on every `accept`, keeping the reassembler a **pure, clock-free** value that
//! is trivially deterministic under test.
//!
//! **Non-money-path** — the reassembled bytes stay opaque here; nothing is signed,
//! inspected, or broadcast.

use std::collections::BTreeMap;
use std::collections::HashMap;

/// Length of a fragment-set id in bytes.
pub const FRAGMENT_ID_LEN: usize = 8;

/// Fixed fragment header length: `fragmentID(8) + index(2) + total(2) + originalType(1)`.
pub const FRAGMENT_HEADER_LEN: usize = FRAGMENT_ID_LEN + 2 + 2 + 1;

/// The default BLE write size; payloads larger than this (minus the header) fragment.
pub const BLE_DEFAULT_FRAGMENT_SIZE: usize = 469;

/// The usable chunk size per fragment under the default BLE write budget.
pub const DEFAULT_CHUNK_SIZE: usize = BLE_DEFAULT_FRAGMENT_SIZE - FRAGMENT_HEADER_LEN;

/// Maximum partial messages a [`Reassembler`] holds at once (oldest evicted past this).
pub const MAX_IN_FLIGHT: usize = 128;

/// How long (ms) a partial message lives before the reassembler discards it.
pub const REASSEMBLY_LIFETIME_MS: u64 = 30_000;

/// One parsed fragment: its set id, position, the set size, the original message type,
/// and this fragment's chunk of the original payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// The shared id tying every fragment of one message together.
    pub id: [u8; FRAGMENT_ID_LEN],
    /// This fragment's 0-based position within the set.
    pub index: u16,
    /// Total number of fragments in the set (`>= 1`).
    pub total: u16,
    /// The [`crate::packet::MessageType`] byte the reassembled payload belongs to.
    pub original_type: u8,
    /// This fragment's slice of the original payload.
    pub chunk: Vec<u8>,
}

/// Serialize a [`Fragment`] to its on-wire payload bytes (header + chunk).
pub fn encode_fragment(f: &Fragment) -> Vec<u8> {
    let mut buf = Vec::with_capacity(FRAGMENT_HEADER_LEN + f.chunk.len());
    buf.extend_from_slice(&f.id);
    buf.extend_from_slice(&f.index.to_be_bytes());
    buf.extend_from_slice(&f.total.to_be_bytes());
    buf.push(f.original_type);
    buf.extend_from_slice(&f.chunk);
    buf
}

/// Parse a fragment payload, returning `None` on any malformed input (panic-free over
/// arbitrary bytes — mesh input is hostile).
pub fn parse_fragment(bytes: &[u8]) -> Option<Fragment> {
    if bytes.len() < FRAGMENT_HEADER_LEN {
        return None;
    }
    let mut id = [0u8; FRAGMENT_ID_LEN];
    id.copy_from_slice(&bytes[..FRAGMENT_ID_LEN]);
    let index = u16::from_be_bytes([bytes[8], bytes[9]]);
    let total = u16::from_be_bytes([bytes[10], bytes[11]]);
    let original_type = bytes[12];
    // A set has at least one fragment and every index must be inside it.
    if total == 0 || index >= total {
        return None;
    }
    let chunk = bytes[FRAGMENT_HEADER_LEN..].to_vec();
    Some(Fragment {
        id,
        index,
        total,
        original_type,
        chunk,
    })
}

/// Split a message of `original_type` into fragment payloads of at most `chunk_size`
/// bytes each, all sharing `id`.
///
/// Always yields at least one fragment (an empty payload → a single empty-chunk
/// fragment), so the round-trip is total. `chunk_size` is clamped to `>= 1`. The result
/// is a `Vec` of ready-to-flood `fragment` packet payloads.
pub fn fragment_message(
    original_type: u8,
    payload: &[u8],
    chunk_size: usize,
    id: [u8; FRAGMENT_ID_LEN],
) -> Vec<Vec<u8>> {
    let chunk_size = chunk_size.max(1);
    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(chunk_size).collect()
    };
    let total = chunks.len().min(u16::MAX as usize) as u16;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            encode_fragment(&Fragment {
                id,
                index: i as u16,
                total,
                original_type,
                chunk: chunk.to_vec(),
            })
        })
        .collect()
}

/// One partial message the reassembler is collecting.
struct Partial {
    total: u16,
    original_type: u8,
    chunks: BTreeMap<u16, Vec<u8>>,
    created_ms: u64,
}

/// A bounded, clock-free reassembler for inbound [`Fragment`]s.
///
/// Bounded to [`MAX_IN_FLIGHT`] partials (oldest evicted) and [`REASSEMBLY_LIFETIME_MS`]
/// lifetime so hostile, never-completing fragment sets cannot exhaust memory. The caller
/// passes a monotonic `now_ms` on every [`accept`](Reassembler::accept).
#[derive(Default)]
pub struct Reassembler {
    partials: HashMap<[u8; FRAGMENT_ID_LEN], Partial>,
}

impl Reassembler {
    /// A fresh, empty reassembler.
    pub fn new() -> Self {
        Reassembler {
            partials: HashMap::new(),
        }
    }

    /// Number of partial messages currently held (test/observability hook).
    pub fn in_flight(&self) -> usize {
        self.partials.len()
    }

    /// Absorb one fragment at logical time `now_ms`. Returns `Some((originalType, bytes))`
    /// the moment a set is complete (the partial is then removed), else `None`.
    ///
    /// Expired partials are GC'd first; a fragment whose `total`/`type` conflicts with an
    /// in-progress set for the same id resets that set (the newer set wins).
    pub fn accept(&mut self, f: Fragment, now_ms: u64) -> Option<(u8, Vec<u8>)> {
        self.gc(now_ms);

        // A conflicting total/type for an existing id means a stale/foreign set — restart.
        if let Some(p) = self.partials.get(&f.id) {
            if p.total != f.total || p.original_type != f.original_type {
                self.partials.remove(&f.id);
            }
        }

        if !self.partials.contains_key(&f.id) {
            self.evict_if_full();
            self.partials.insert(
                f.id,
                Partial {
                    total: f.total,
                    original_type: f.original_type,
                    chunks: BTreeMap::new(),
                    created_ms: now_ms,
                },
            );
        }

        let p = self.partials.get_mut(&f.id)?;
        p.chunks.insert(f.index, f.chunk);

        if p.chunks.len() as u16 != p.total {
            return None;
        }
        // Complete: concatenate chunks in index order and emit.
        let original_type = p.original_type;
        let mut out = Vec::new();
        for chunk in p.chunks.values() {
            out.extend_from_slice(chunk);
        }
        self.partials.remove(&f.id);
        Some((original_type, out))
    }

    /// Drop partials older than [`REASSEMBLY_LIFETIME_MS`].
    fn gc(&mut self, now_ms: u64) {
        self.partials
            .retain(|_, p| now_ms.saturating_sub(p.created_ms) < REASSEMBLY_LIFETIME_MS);
    }

    /// Evict the oldest partial when at capacity (so an insert stays within the bound).
    fn evict_if_full(&mut self) {
        if self.partials.len() < MAX_IN_FLIGHT {
            return;
        }
        if let Some(oldest) = self
            .partials
            .iter()
            .min_by_key(|(_, p)| p.created_ms)
            .map(|(id, _)| *id)
        {
            self.partials.remove(&oldest);
        }
    }
}

// --- engine glue (moved from `engine.rs` for the 800-line guard) ---------------------

use crate::engine::{dispatch_packet, relay_key, relay_onward, remember, WorkerCtx, WorkerState};
use crate::packet::{MessageType, Packet, PEER_ID_LEN};

/// G6 fragment path: carry the fragment onward (it's a normal flooded packet) and feed
/// its chunk to the bounded [`Reassembler`]. When a set completes, the original message
/// is reconstructed with **TTL zeroed** (PROTOCOL.md: "reassembled TTL zeroed") and
/// dispatched **locally only** — it is never re-flooded, since the individual fragments
/// already propagated it.
pub(crate) fn handle_fragment(
    ctx: &WorkerCtx,
    packet: Packet,
    src: Option<&str>,
    st: &mut WorkerState,
) {
    if !st.relay_seen.insert(relay_key(&packet)) {
        return;
    }
    // G7: cache the fragment packet so gossip-sync can replay it to a rejoining peer.
    remember(ctx, st, &packet);
    // Parse the chunk before we move `packet` into the relay step.
    let parsed = parse_fragment(&packet.payload);
    let now = st.now_ms();
    relay_onward(ctx, packet, src, st);

    let Some(fragment) = parsed else {
        return; // malformed fragment header — carried but not reassembled.
    };
    if let Some((original_type, payload)) = st.reassembler.accept(fragment, now) {
        if let Some(msg_type) = MessageType::from_u8(original_type) {
            // Reconstruct the original message, TTL zeroed so it is delivered locally and
            // never rebroadcast (the fragments already did the flooding).
            let mut reassembled = Packet::new(msg_type, [0u8; PEER_ID_LEN], payload);
            reassembled.ttl = 0;
            reassembled.timestamp_ms = st.now_ms();
            dispatch_reassembled(ctx, reassembled, st);
        }
    }
}

/// Dispatch a reassembled (TTL-0) message through the normal type handlers. `relay_onward`
/// drops it at the hop floor, so this only ever *delivers* (gateway submit / origin
/// settle), never re-floods. A reassembled fragment-of-a-fragment is impossible
/// (`fragment_message` never wraps a `fragment` payload), so this can't recurse.
fn dispatch_reassembled(ctx: &WorkerCtx, packet: Packet, st: &mut WorkerState) {
    // TTL is already zeroed, so `relay_onward` drops at the hop floor — this only ever
    // *delivers* (gateway submit / origin settle / cache), never re-floods.
    dispatch_packet(ctx, packet, None, st);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reassemble_all(frags: &[Vec<u8>], base_now: u64) -> Option<(u8, Vec<u8>)> {
        let mut r = Reassembler::new();
        let mut out = None;
        for (i, fp) in frags.iter().enumerate() {
            let f = parse_fragment(fp).expect("our own fragment parses");
            if let Some(done) = r.accept(f, base_now + i as u64) {
                out = Some(done);
            }
        }
        out
    }

    #[test]
    fn fragment_then_reassemble_round_trips() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let frags = fragment_message(0x30, &payload, 64, [9; 8]);
        assert!(
            frags.len() > 1,
            "a 1000-B payload at 64-B chunks must fragment"
        );
        assert_eq!(reassemble_all(&frags, 1000), Some((0x30, payload)));
    }

    #[test]
    fn single_fragment_for_small_payload() {
        let payload = b"small".to_vec();
        let frags = fragment_message(0x31, &payload, DEFAULT_CHUNK_SIZE, [1; 8]);
        assert_eq!(frags.len(), 1);
        assert_eq!(reassemble_all(&frags, 0), Some((0x31, payload)));
    }

    #[test]
    fn empty_payload_round_trips_as_one_fragment() {
        let frags = fragment_message(0x21, &[], 10, [2; 8]);
        assert_eq!(frags.len(), 1);
        assert_eq!(reassemble_all(&frags, 0), Some((0x21, Vec::new())));
    }

    #[test]
    fn out_of_order_fragments_reassemble() {
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let mut frags = fragment_message(0x30, &payload, 32, [7; 8]);
        frags.reverse();
        assert_eq!(reassemble_all(&frags, 500), Some((0x30, payload)));
    }

    #[test]
    fn parse_fragment_rejects_malformed() {
        assert!(parse_fragment(&[]).is_none());
        assert!(parse_fragment(&[0u8; FRAGMENT_HEADER_LEN - 1]).is_none());
        // total = 0 is invalid.
        let bad = Fragment {
            id: [0; 8],
            index: 0,
            total: 0,
            original_type: 0x30,
            chunk: vec![],
        };
        // Hand-build bytes with total 0 (encode_fragment would happily emit it).
        let bytes = encode_fragment(&bad);
        assert!(parse_fragment(&bytes).is_none());
        // index >= total is invalid.
        let bad2 = encode_fragment(&Fragment {
            id: [0; 8],
            index: 3,
            total: 3,
            original_type: 0x30,
            chunk: vec![],
        });
        assert!(parse_fragment(&bad2).is_none());
    }

    #[test]
    fn incomplete_set_returns_none_and_stays_in_flight() {
        let payload: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let frags = fragment_message(0x30, &payload, 32, [4; 8]);
        let mut r = Reassembler::new();
        // Feed all but the last fragment.
        for fp in &frags[..frags.len() - 1] {
            assert!(r.accept(parse_fragment(fp).unwrap(), 100).is_none());
        }
        assert_eq!(r.in_flight(), 1);
    }

    #[test]
    fn expired_partials_are_gced() {
        let payload: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let frags = fragment_message(0x30, &payload, 32, [5; 8]);
        let mut r = Reassembler::new();
        r.accept(parse_fragment(&frags[0]).unwrap(), 0);
        assert_eq!(r.in_flight(), 1);
        // A fragment arriving past the lifetime GCs the stale partial, then starts fresh.
        r.accept(
            parse_fragment(&frags[0]).unwrap(),
            REASSEMBLY_LIFETIME_MS + 1,
        );
        assert_eq!(r.in_flight(), 1);
    }

    #[test]
    fn reassembler_is_bounded_to_max_in_flight() {
        let mut r = Reassembler::new();
        // Open MAX_IN_FLIGHT + 50 distinct two-fragment sets, completing none.
        for i in 0..(MAX_IN_FLIGHT + 50) {
            let mut id = [0u8; 8];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            let frags = fragment_message(0x30, &[1, 2, 3, 4], 2, id);
            r.accept(parse_fragment(&frags[0]).unwrap(), i as u64);
        }
        assert!(
            r.in_flight() <= MAX_IN_FLIGHT,
            "reassembler exceeded its bound"
        );
    }
}
