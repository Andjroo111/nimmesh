//! # header — the Albatross micro-block header codec (issue #5, verify step 3)
//!
//! The chain primitive the balance-proof verify path reads: a phone that received a
//! `nimiqBalanceProof` (`0x37`) must extract the **`state_root`** from the serialized
//! block header it carries, after [`crate::balance_proof::bind_to_beacon`] has proven
//! the header hashes to the head-beacon block hash. This module owns the byte layout.
//!
//! ## The layout, cited from `core-rs-albatross`
//!
//! An Albatross block hash is `Blake2b-256` of the header's plain `nimiq-serde`
//! serialization (`hash_derive`'s `SerializeContent` writes exactly the serde bytes),
//! and `nimiq-serde` is the **postcard** wire format via Nimiq's published
//! `postcard-bytes` fork: `u16`/`u32`/`u64` as LEB128 varints, `Vec<u8>` varint-length-
//! prefixed, fixed arrays raw. `MicroHeader` (`primitives/block/src/micro_block.rs`),
//! field order:
//!
//! ```text
//! network(1, the repr byte — custom serde writes the VALUE, e.g. testnet 5)
//! version(varint u16) | blockNumber(varint u32) | timestamp(varint u64, ms)
//! parentHash(32) | seed(96, VXEdDSA VrfSeed) | extraData(varint len ≤ 32, bytes)
//! stateRoot(32) | bodyRoot(32, Blake2s) | diffRoot(32) | historyRoot(32)
//! ```
//!
//! Fidelity is enforced two ways: the encoding layer is **differential-tested against
//! `postcard-bytes` itself** (the exact crate the chain serializes with, as a
//! dev-dependency, over a mirror struct), and the field order/types above are cited to
//! source. What is still pending is a byte vector captured from a REAL node (the JSON-RPC
//! cannot produce one — its `Block` type omits `diffRoot`, so a header can never be
//! rebuilt from RPC JSON); until such a vector is committed, the verify path treats a
//! parsed `state_root` as usable only behind the full bind-then-trie-verify chain, where
//! a wrong parse can only fail closed.
//!
//! **Macro headers are not this layout.** A head beacon can point at a macro block (once
//! per batch); a macro header fed to [`decode_micro_header`] is overwhelmingly rejected
//! by the strict bounds here, and in the worst case yields a garbage `state_root` that
//! fails trie verification — untrusted, never wrong. Real macro support is staged.
//!
//! **Non-money-path.** Public chain data in, public fields out; nothing signs or
//! broadcasts. Decoding is bounds-checked and panic-free: mesh input is hostile.

/// Length of a Blake2b/Blake2s hash field in a header, in bytes.
pub const HEADER_HASH_LEN: usize = 32;

/// Length of a serialized `VrfSeed` (a VXEdDSA signature), in bytes.
pub const VRF_SEED_LEN: usize = 96;

/// Upstream cap on a header's `extra_data` ("simply 32 raw bytes").
pub const MAX_EXTRA_DATA_LEN: usize = 32;

/// The smallest possible serialized micro header: every varint one byte, empty
/// `extra_data`.
pub const MICRO_HEADER_MIN_LEN: usize =
    1 + 1 + 1 + 1 + HEADER_HASH_LEN + VRF_SEED_LEN + 1 + 4 * HEADER_HASH_LEN;

/// The largest possible serialized micro header: maximal varints (u16→3 B, u32→5 B,
/// u64→10 B), full 32-byte `extra_data`.
pub const MICRO_HEADER_MAX_LEN: usize = 1
    + 3
    + 5
    + 10
    + HEADER_HASH_LEN
    + VRF_SEED_LEN
    + (1 + MAX_EXTRA_DATA_LEN)
    + 4 * HEADER_HASH_LEN;

/// The Albatross network-id repr bytes the chain accepts (`primitives/src/networks.rs`).
/// A decoded header on any other byte is rejected, mirroring upstream deserialization.
pub const KNOWN_NETWORK_IDS: &[u8] = &[1, 2, 3, 4, 42, 5, 6, 7, 24];

/// A decoded Albatross micro-block header — the fields as the chain serializes them.
/// `state_root` is what the balance-proof verify path is after; the rest are carried so
/// the caller can cross-check (`block_number` vs the beacon height, `network` vs the
/// node's network) without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroHeader {
    /// The network repr byte (testnet `5`, mainnet `24`).
    pub network: u8,
    /// Protocol version (hard-fork counter).
    pub version: u16,
    /// The block height — must equal the bound beacon's height.
    pub block_number: u32,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
    /// Hash of the preceding block's header.
    pub parent_hash: [u8; HEADER_HASH_LEN],
    /// The block's VRF seed (a VXEdDSA signature; opaque here).
    pub seed: [u8; VRF_SEED_LEN],
    /// Free-form extra data, at most [`MAX_EXTRA_DATA_LEN`] bytes.
    pub extra_data: Vec<u8>,
    /// **The accounts-trie root at this block** — what a `TrieProof` verifies against.
    pub state_root: [u8; HEADER_HASH_LEN],
    /// The body commitment (Blake2s).
    pub body_root: [u8; HEADER_HASH_LEN],
    /// The trie-diff proof root.
    pub diff_root: [u8; HEADER_HASH_LEN],
    /// The epoch history-tree root.
    pub history_root: [u8; HEADER_HASH_LEN],
}

// --- postcard varints (LEB128, little-endian groups) ---------------------------------

/// Append `value` as a minimal LEB128 varint (what postcard emits).
fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Read a LEB128 varint of at most `max_bytes` (postcard's per-type bound: u16→3,
/// u32→5, u64→10), advancing `off`. `None` on truncation, on an over-long varint, or on
/// overflow past `max_value` — the same inputs postcard rejects.
fn read_varint(bytes: &[u8], off: &mut usize, max_bytes: usize, max_value: u64) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..max_bytes {
        let byte = *bytes.get(*off)?;
        *off += 1;
        // The final allowed byte must not overflow the type when shifted into place.
        let shifted = (u64::from(byte & 0x7F)).checked_shl((7 * i) as u32)?;
        value = value.checked_add(shifted)?;
        if byte & 0x80 == 0 {
            if value > max_value {
                return None;
            }
            return Some(value);
        }
    }
    None // continuation bit still set past the type's byte budget.
}

/// Copy an exact-length field out of `bytes` at `off`, advancing it.
fn read_array<const N: usize>(bytes: &[u8], off: &mut usize) -> Option<[u8; N]> {
    let end = off.checked_add(N)?;
    let slice = bytes.get(*off..end)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    *off = end;
    Some(out)
}

/// Serialize a [`MicroHeader`] exactly as the chain does (postcard field order in the
/// module docs). `Blake2b-256` of this output is the block hash. `extra_data` longer
/// than [`MAX_EXTRA_DATA_LEN`] is truncated to the cap — an encoder input that long is a
/// caller bug, and the decoder rejects it anyway; tests use valid lengths.
pub fn encode_micro_header(h: &MicroHeader) -> Vec<u8> {
    let extra = &h.extra_data[..h.extra_data.len().min(MAX_EXTRA_DATA_LEN)];
    let mut out = Vec::with_capacity(MICRO_HEADER_MAX_LEN);
    out.push(h.network);
    write_varint(&mut out, u64::from(h.version));
    write_varint(&mut out, u64::from(h.block_number));
    write_varint(&mut out, h.timestamp);
    out.extend_from_slice(&h.parent_hash);
    out.extend_from_slice(&h.seed);
    write_varint(&mut out, extra.len() as u64);
    out.extend_from_slice(extra);
    out.extend_from_slice(&h.state_root);
    out.extend_from_slice(&h.body_root);
    out.extend_from_slice(&h.diff_root);
    out.extend_from_slice(&h.history_root);
    out
}

/// Decode a serialized micro header. `None` unless the input parses field-exactly,
/// carries a known network byte, respects the `extra_data` cap, and is consumed to the
/// last byte — hostile-input discipline throughout, panic-free.
pub fn decode_micro_header(bytes: &[u8]) -> Option<MicroHeader> {
    if bytes.len() < MICRO_HEADER_MIN_LEN || bytes.len() > MICRO_HEADER_MAX_LEN {
        return None;
    }
    let mut off = 0usize;
    let network = *bytes.first()?;
    off += 1;
    if !KNOWN_NETWORK_IDS.contains(&network) {
        return None;
    }
    let version = read_varint(bytes, &mut off, 3, u64::from(u16::MAX))? as u16;
    let block_number = read_varint(bytes, &mut off, 5, u64::from(u32::MAX))? as u32;
    let timestamp = read_varint(bytes, &mut off, 10, u64::MAX)?;
    let parent_hash = read_array::<HEADER_HASH_LEN>(bytes, &mut off)?;
    let seed = read_array::<VRF_SEED_LEN>(bytes, &mut off)?;
    let extra_len = read_varint(bytes, &mut off, 2, MAX_EXTRA_DATA_LEN as u64)? as usize;
    let extra_end = off.checked_add(extra_len)?;
    let extra_data = bytes.get(off..extra_end)?.to_vec();
    off = extra_end;
    let state_root = read_array::<HEADER_HASH_LEN>(bytes, &mut off)?;
    let body_root = read_array::<HEADER_HASH_LEN>(bytes, &mut off)?;
    let diff_root = read_array::<HEADER_HASH_LEN>(bytes, &mut off)?;
    let history_root = read_array::<HEADER_HASH_LEN>(bytes, &mut off)?;
    if off != bytes.len() {
        return None; // trailing bytes — not this layout (a macro header, or garbage).
    }
    Some(MicroHeader {
        network,
        version,
        block_number,
        timestamp,
        parent_hash,
        seed,
        extra_data,
        state_root,
        body_root,
        diff_root,
        history_root,
    })
}

/// The accounts-trie `state_root` of a serialized micro header, or `None` if the bytes
/// do not parse as one. The step-3 entry point for the balance-proof verify path — call
/// only on a header that already **bound** to the head beacon
/// ([`crate::balance_proof::bind_to_beacon`] returned `Bound`), so a parse of unbound
/// bytes cannot launder a fake root into the trie check.
pub fn state_root_of_header(bytes: &[u8]) -> Option<[u8; HEADER_HASH_LEN]> {
    Some(decode_micro_header(bytes)?.state_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MicroHeader {
        MicroHeader {
            network: 5, // testnet
            version: 2,
            block_number: 4_600_123,
            timestamp: 1_756_100_000_123,
            parent_hash: [0x11; HEADER_HASH_LEN],
            seed: [0x22; VRF_SEED_LEN],
            extra_data: vec![0xAA, 0xBB, 0xCC],
            state_root: [0x33; HEADER_HASH_LEN],
            body_root: [0x44; HEADER_HASH_LEN],
            diff_root: [0x55; HEADER_HASH_LEN],
            history_root: [0x66; HEADER_HASH_LEN],
        }
    }

    #[test]
    fn roundtrip_is_field_exact() {
        let h = sample();
        let bytes = encode_micro_header(&h);
        assert!(bytes.len() >= MICRO_HEADER_MIN_LEN && bytes.len() <= MICRO_HEADER_MAX_LEN);
        assert_eq!(decode_micro_header(&bytes), Some(h.clone()));
        assert_eq!(state_root_of_header(&bytes), Some(h.state_root));
    }

    #[test]
    fn min_and_max_shapes_roundtrip() {
        let mut min = sample();
        min.version = 0;
        min.block_number = 0;
        min.timestamp = 0;
        min.extra_data = vec![];
        let bytes = encode_micro_header(&min);
        assert_eq!(bytes.len(), MICRO_HEADER_MIN_LEN);
        assert_eq!(decode_micro_header(&bytes), Some(min));

        let mut max = sample();
        max.version = u16::MAX;
        max.block_number = u32::MAX;
        max.timestamp = u64::MAX;
        max.extra_data = vec![0xEE; MAX_EXTRA_DATA_LEN];
        let bytes = encode_micro_header(&max);
        assert_eq!(bytes.len(), MICRO_HEADER_MAX_LEN);
        assert_eq!(decode_micro_header(&bytes), Some(max));
    }

    #[test]
    fn decoder_rejects_hostile_input() {
        let good = encode_micro_header(&sample());
        assert_eq!(decode_micro_header(&[]), None);
        for cut in [1, MICRO_HEADER_MIN_LEN - 1, good.len() - 1] {
            assert_eq!(decode_micro_header(&good[..cut]), None, "cut at {cut}");
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_micro_header(&long), None);
        // Unknown network byte.
        let mut alien = good.clone();
        alien[0] = 99;
        assert_eq!(decode_micro_header(&alien), None);
        // An extra_data length claiming past the cap.
        let mut h = sample();
        h.extra_data = vec![0; MAX_EXTRA_DATA_LEN];
        let mut bytes = encode_micro_header(&h);
        let extra_len_off = 1
            + varint_len(u64::from(h.version))
            + varint_len(u64::from(h.block_number))
            + varint_len(h.timestamp)
            + HEADER_HASH_LEN
            + VRF_SEED_LEN;
        bytes[extra_len_off] = (MAX_EXTRA_DATA_LEN + 1) as u8;
        assert_eq!(decode_micro_header(&bytes), None);
    }

    fn varint_len(v: u64) -> usize {
        let mut out = Vec::new();
        write_varint(&mut out, v);
        out.len()
    }

    #[test]
    fn overlong_varints_are_rejected_like_postcard() {
        // version = 1 encoded non-minimally over 4 bytes: continuation past u16's
        // 3-byte budget must die, exactly as postcard's BadVarint does.
        let mut bytes = encode_micro_header(&sample());
        // Splice an over-long varint into the version position (offset 1).
        let mut evil = vec![bytes[0], 0x81, 0x80, 0x80, 0x00];
        evil.extend_from_slice(&bytes.split_off(1)[varint_len(u64::from(sample().version))..]);
        assert_eq!(decode_micro_header(&evil), None);
    }

    /// The fidelity anchor: our hand-rolled encoding vs `postcard-bytes` — the exact
    /// serde crate `nimiq-serde` wraps on chain — over a mirror of the upstream struct
    /// (same field order and serde shapes: `network` written as its repr byte, hashes
    /// and the seed as `FixedSizeByteArray`, `extra_data` as a `Vec<u8>`). Byte-for-byte
    /// across the varint-width matrix, so the encoding layer cannot silently diverge
    /// from the chain's.
    #[test]
    fn encoding_matches_postcard_bytes_exactly() {
        #[derive(serde::Serialize)]
        struct Mirror {
            network: u8,
            version: u16,
            block_number: u32,
            timestamp: u64,
            parent_hash: postcard::FixedSizeByteArray<32>,
            seed: postcard::FixedSizeByteArray<96>,
            extra_data: Vec<u8>,
            state_root: postcard::FixedSizeByteArray<32>,
            body_root: postcard::FixedSizeByteArray<32>,
            diff_root: postcard::FixedSizeByteArray<32>,
            history_root: postcard::FixedSizeByteArray<32>,
        }

        let cases: Vec<MicroHeader> = vec![
            sample(),
            {
                let mut h = sample();
                h.version = 0;
                h.block_number = 0;
                h.timestamp = 0;
                h.extra_data = vec![];
                h
            },
            {
                let mut h = sample();
                h.version = u16::MAX;
                h.block_number = u32::MAX;
                h.timestamp = u64::MAX;
                h.extra_data = vec![0x7F; MAX_EXTRA_DATA_LEN];
                h
            },
            {
                // Each varint at a width boundary (one-byte → two-byte edges).
                let mut h = sample();
                h.network = 24; // mainnet byte, still one raw byte
                h.version = 128;
                h.block_number = 16_384;
                h.timestamp = 2_097_152;
                h.extra_data = vec![0x01];
                h
            },
        ];

        for (i, h) in cases.iter().enumerate() {
            let mirror = Mirror {
                network: h.network,
                version: h.version,
                block_number: h.block_number,
                timestamp: h.timestamp,
                parent_hash: h.parent_hash.into(),
                seed: h.seed.into(),
                extra_data: h.extra_data.clone(),
                state_root: h.state_root.into(),
                body_root: h.body_root.into(),
                diff_root: h.diff_root.into(),
                history_root: h.history_root.into(),
            };
            let theirs = postcard::to_allocvec(&mirror).expect("postcard serializes");
            let ours = encode_micro_header(h);
            assert_eq!(ours, theirs, "case {i} diverged from postcard-bytes");
            // And what postcard wrote, our strict decoder reads back.
            assert_eq!(decode_micro_header(&theirs).as_ref(), Some(h), "case {i}");
        }
    }
}
