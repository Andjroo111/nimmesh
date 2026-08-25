//! # balance_proof — the trustless balance upgrade, wire envelope + head binding (G15 part 3)
//!
//! [`crate::balance`] ships balance-over-mesh as **unverified / last-known**: a gateway
//! states a balance at a head height and the app labels it untrusted. This module is the
//! first slice of the trustless upgrade (issue #5, `docs/BALANCE-PROOF.md`): carry an
//! Albatross **accounts proof** as the response payload, so the receiving phone checks
//! "address A held X NIM as of block H" against a block hash it already trusts from the
//! G9 head beacon — without trusting the gateway or any relay in between.
//!
//! The design principle (owed to a sharp community critique): the mesh protocol is only
//! the **envelope**; the thing that makes a payload trustworthy must be a **chain
//! primitive**, not an invention of this repo. The payload here is exactly that pair of
//! chain primitives: a serialized Albatross **block header** and a serialized
//! **`TrieProof`** as `core-rs-albatross` defines them — this module never re-invents
//! either, it only frames them for the mesh and binds them to the beacon.
//!
//! ## What is implemented here vs. staged
//!
//! | step | what | status |
//! | ---- | ---- | ------ |
//! | 1 | `nimiqBalanceProof` (`0x37`) wire envelope, exact + panic-free | **here** |
//! | 2 | header ↔ beacon binding: `Blake2b-256(header) == beacon.blockHash` | **here** |
//! | 3 | `stateRoot` extraction from the header (postcard layout) | **here** ([`state_root_for_bound_proof`]) |
//! | 4 | the `TrieProof` walk against `stateRoot` (post-order, Blake2b) | staged |
//!
//! Step 3 rides [`crate::nimiq::header`], whose encoding is differential-tested against
//! `postcard-bytes` (the exact serde crate the chain uses). Step 4 still needs a proof
//! *source* (the JSON-RPC surface has none — see the design doc) and real-node fixture
//! vectors. Until it lands, nothing here upgrades a cached balance to verified:
//! decoding + relaying a `0x37` is safe today, and registering the type NOW means every
//! shipped relay carries proofs blindly the day gateways start emitting them.
//!
//! ## On-wire payload (`0x37`, big-endian)
//!
//! ```text
//! address(20) | headHeight(4) | networkId(1)
//!            | headerLen(2)  | header(headerLen)   — serialized Albatross block header
//!            | proofLen(2)   | proof(proofLen)     — serialized Albatross TrieProof
//! ```
//!
//! A realistic payload is a few hundred bytes to a few KB — past the 256-byte BLE frame —
//! so it rides the proven G6 fragment path exactly like a `0x36` history response (the
//! round-trip is asserted in this module's tests). Decoders are length-exact, cap-checked
//! and panic-free: mesh input is hostile (GOAL.md core value #6).
//!
//! **Non-money-path.** Read-only public state: no keys, no signing, no broadcast. A proof
//! shrinks *acceptance* risk; it never creates finality — see `docs/BALANCE-PROOF.md` for
//! what the freshness window still leaves open.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use crate::beacon::{HeadBeacon, BLOCK_HASH_LEN};
use crate::nimiq::address::{Address, ADDRESS_LEN};

/// Fixed part of a [`BalanceProof`] payload: `address(20) | headHeight(4) | networkId(1)
/// | headerLen(2) | proofLen(2)`.
pub const BALANCE_PROOF_FIXED_LEN: usize = ADDRESS_LEN + 4 + 1 + 2 + 2;

/// Cap on the serialized block header a proof may carry. A real Albatross micro/macro
/// header (hashes, VRF seed, varints, bounded extra-data) stays well under this; anything
/// larger is hostile input.
pub const MAX_HEADER_LEN: usize = 1024;

/// Cap on the serialized `TrieProof`. One account's path through the accounts trie is a
/// handful of radix-16 nodes (each ≤ 16 child hashes); multiple KB covers deep tries with
/// generous margin; anything larger is hostile input.
pub const MAX_PROOF_LEN: usize = 8 * 1024;

/// A gateway → mesh `nimiqBalanceProof` (`0x37`): the two chain primitives that make a
/// balance claim checkable offline, framed for the mesh.
///
/// `header` and `proof` are **opaque bytes** at this layer — they are `core-rs-albatross`
/// serializations, carried verbatim. The only structure this module interprets is the
/// binding: `Blake2b-256(header)` must equal the head-beacon block hash (see
/// [`bind_to_beacon`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceProof {
    /// The address the proof speaks for (so a node can match it to its query).
    pub address: Address,
    /// The block height the proof was built at — must equal the bound beacon's height.
    pub head_height: u32,
    /// The Albatross network-id byte (testnet `5`); a node rejects a mismatch.
    pub network_id: u8,
    /// The serialized Albatross block header at `head_height` (opaque; its Blake2b-256
    /// is the block hash, and it contains the `stateRoot` the trie proof verifies against).
    pub header: Vec<u8>,
    /// The serialized Albatross `TrieProof` for `address` (opaque until the step-4 walk).
    pub proof: Vec<u8>,
}

/// Encode a [`BalanceProof`] payload (big-endian, layout in the module docs).
pub fn encode_balance_proof(p: &BalanceProof) -> Vec<u8> {
    let mut out = Vec::with_capacity(BALANCE_PROOF_FIXED_LEN + p.header.len() + p.proof.len());
    out.extend_from_slice(p.address.as_bytes());
    out.extend_from_slice(&p.head_height.to_be_bytes());
    out.push(p.network_id);
    out.extend_from_slice(&(p.header.len() as u16).to_be_bytes());
    out.extend_from_slice(&p.header);
    out.extend_from_slice(&(p.proof.len() as u16).to_be_bytes());
    out.extend_from_slice(&p.proof);
    out
}

/// Decode a [`BalanceProof`] payload. `None` unless the input is length-exact, both
/// length fields are within [`MAX_HEADER_LEN`] / [`MAX_PROOF_LEN`], and both parts are
/// non-empty (a proof without a header — or vice versa — can never bind, so it is
/// malformed, not merely useless).
pub fn decode_balance_proof(bytes: &[u8]) -> Option<BalanceProof> {
    if bytes.len() < BALANCE_PROOF_FIXED_LEN {
        return None;
    }
    let mut addr = [0u8; ADDRESS_LEN];
    addr.copy_from_slice(&bytes[..ADDRESS_LEN]);
    let mut h = [0u8; 4];
    h.copy_from_slice(&bytes[ADDRESS_LEN..ADDRESS_LEN + 4]);
    let head_height = u32::from_be_bytes(h);
    let network_id = bytes[ADDRESS_LEN + 4];

    let mut off = ADDRESS_LEN + 5;
    let header_len = u16::from_be_bytes([bytes[off], bytes[off + 1]]) as usize;
    off += 2;
    if header_len == 0 || header_len > MAX_HEADER_LEN {
        return None;
    }
    // `off + header_len + 2` must fit before we slice (hostile-length discipline).
    if bytes.len() < off + header_len + 2 {
        return None;
    }
    let header = bytes[off..off + header_len].to_vec();
    off += header_len;
    let proof_len = u16::from_be_bytes([bytes[off], bytes[off + 1]]) as usize;
    off += 2;
    if proof_len == 0 || proof_len > MAX_PROOF_LEN {
        return None;
    }
    if bytes.len() != off + proof_len {
        return None; // trailing or missing bytes — length-exact only.
    }
    let proof = bytes[off..].to_vec();

    Some(BalanceProof {
        address: Address::from_bytes(addr),
        head_height,
        network_id,
        header,
        proof,
    })
}

/// `Blake2b-256(header)` — the Albatross block hash of a serialized header. This is the
/// same hash family the codebase already uses for address derivation and HTLC roots.
pub fn block_hash_of_header(header: &[u8]) -> [u8; BLOCK_HASH_LEN] {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(header);
    let digest = hasher.finalize();
    let mut out = [0u8; BLOCK_HASH_LEN];
    out.copy_from_slice(&digest);
    out
}

/// The outcome of binding a [`BalanceProof`] to the freshest head beacon a node holds.
/// Only [`BindVerdict::Bound`] may ever feed the staged verify steps (3–4); every other
/// verdict leaves the balance exactly as untrusted as it is today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindVerdict {
    /// The header hashes to the beacon's block hash at the beacon's height on the
    /// beacon's network: the proof is anchored to a head this node already trusts.
    Bound,
    /// The proof's network id differs from the beacon's.
    WrongNetwork,
    /// The proof is older than the cached head (issue #5: "reject proofs older than the
    /// cached head") — a replayed or long-delayed proof, useless even if internally valid.
    StaleHeight,
    /// The proof claims a head this node has not heard a beacon for yet. Unverifiable
    /// (not necessarily malicious — the beacon may simply not have arrived).
    AheadOfBeacon,
    /// The beacon's block hash is still zeroed (the G8 RPC seam predates `getBlock`), so
    /// there is nothing to bind against. The RPC-side prerequisite in the design doc.
    BeaconUnhashed,
    /// Heights and network match but `Blake2b-256(header)` is not the beacon's hash: the
    /// header is forged, corrupted, or from a competing fork.
    HeaderMismatch,
}

/// Step 3: the accounts-trie `state_root` a **bound** proof's header commits to — the
/// root the step-4 `TrieProof` walk will verify against. `Some` only when
/// [`bind_to_beacon`] returns [`BindVerdict::Bound`] AND the parsed header's own
/// `block_number`/`network` agree with the proof envelope (a bound hash already implies
/// an honest header, so a disagreement here means a malformed envelope — reject).
/// `None` also when the header is not a parseable **micro** header (a macro-block head
/// is not supported yet); the balance then simply stays untrusted — fail-closed.
pub fn state_root_for_bound_proof(
    proof: &BalanceProof,
    beacon: &HeadBeacon,
) -> Option<[u8; BLOCK_HASH_LEN]> {
    if bind_to_beacon(proof, beacon) != BindVerdict::Bound {
        return None;
    }
    let header = crate::nimiq::header::decode_micro_header(&proof.header)?;
    if header.block_number != proof.head_height || header.network != proof.network_id {
        return None;
    }
    Some(header.state_root)
}

/// Bind a proof to the freshest head beacon: same network, same height, and the header
/// must hash to the beacon's (non-zero) block hash. Order matters — the cheap integer
/// checks run before the hash.
pub fn bind_to_beacon(proof: &BalanceProof, beacon: &HeadBeacon) -> BindVerdict {
    if proof.network_id != beacon.network_id {
        return BindVerdict::WrongNetwork;
    }
    if proof.head_height < beacon.height {
        return BindVerdict::StaleHeight;
    }
    if proof.head_height > beacon.height {
        return BindVerdict::AheadOfBeacon;
    }
    if beacon.block_hash == [0u8; BLOCK_HASH_LEN] {
        return BindVerdict::BeaconUnhashed;
    }
    if block_hash_of_header(&proof.header) != beacon.block_hash {
        return BindVerdict::HeaderMismatch;
    }
    BindVerdict::Bound
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::{fragment_message, Reassembler};
    use crate::packet::MessageType;

    fn addr(b: u8) -> Address {
        Address::from_bytes([b; ADDRESS_LEN])
    }

    fn sample(header_len: usize, proof_len: usize) -> BalanceProof {
        BalanceProof {
            address: addr(0x42),
            head_height: 4_500_000,
            network_id: 5,
            header: (0..header_len).map(|i| i as u8).collect(),
            proof: (0..proof_len).map(|i| (i * 7) as u8).collect(),
        }
    }

    #[test]
    fn roundtrip_is_exact() {
        let p = sample(320, 2048);
        let bytes = encode_balance_proof(&p);
        assert_eq!(
            bytes.len(),
            BALANCE_PROOF_FIXED_LEN + p.header.len() + p.proof.len()
        );
        assert_eq!(decode_balance_proof(&bytes), Some(p));
    }

    #[test]
    fn decoder_rejects_hostile_lengths() {
        let good = encode_balance_proof(&sample(64, 128));
        // Truncations at every boundary, and one extra byte.
        assert_eq!(decode_balance_proof(&[]), None);
        assert_eq!(
            decode_balance_proof(&good[..BALANCE_PROOF_FIXED_LEN - 1]),
            None
        );
        assert_eq!(decode_balance_proof(&good[..good.len() - 1]), None);
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_balance_proof(&long), None);
        // A headerLen that points past the buffer must not panic or decode.
        let mut lying = good.clone();
        lying[ADDRESS_LEN + 5] = 0xFF;
        lying[ADDRESS_LEN + 6] = 0xFF;
        assert_eq!(decode_balance_proof(&lying), None);
    }

    #[test]
    fn decoder_rejects_empty_and_oversized_parts() {
        let mut empty_header = sample(1, 8);
        empty_header.header.clear();
        assert_eq!(
            decode_balance_proof(&encode_balance_proof(&empty_header)),
            None
        );
        let mut empty_proof = sample(8, 1);
        empty_proof.proof.clear();
        assert_eq!(
            decode_balance_proof(&encode_balance_proof(&empty_proof)),
            None
        );
        assert_eq!(
            decode_balance_proof(&encode_balance_proof(&sample(MAX_HEADER_LEN + 1, 8))),
            None
        );
        assert_eq!(
            decode_balance_proof(&encode_balance_proof(&sample(8, MAX_PROOF_LEN + 1))),
            None
        );
        // The caps themselves are accepted.
        let at_cap = sample(MAX_HEADER_LEN, MAX_PROOF_LEN);
        assert_eq!(
            decode_balance_proof(&encode_balance_proof(&at_cap)),
            Some(at_cap)
        );
    }

    #[test]
    fn block_hash_matches_blake2b256_reference_vectors() {
        // Independently computed (python hashlib.blake2b, digest_size=32) so the crate
        // is checked against the algorithm, not against itself.
        let abc = block_hash_of_header(b"abc");
        assert_eq!(
            crate::nimiq::hex::bytes_to_hex(&abc),
            "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319"
        );
        let seq: Vec<u8> = (0u8..64).collect();
        assert_eq!(
            crate::nimiq::hex::bytes_to_hex(&block_hash_of_header(&seq)),
            "10d8e6d534b00939843fe9dcc4dae48cdf008f6b8b2b82b156f5404d874887f5"
        );
    }

    #[test]
    fn binding_walks_every_verdict() {
        let p = sample(200, 512);
        let mut beacon = HeadBeacon::new(p.head_height, p.network_id);
        // Beacon hash still zeroed (today's G8 seam) → explicitly unbindable.
        assert_eq!(bind_to_beacon(&p, &beacon), BindVerdict::BeaconUnhashed);
        // Hash present and correct → Bound.
        beacon.block_hash = block_hash_of_header(&p.header);
        assert_eq!(bind_to_beacon(&p, &beacon), BindVerdict::Bound);
        // A forged/corrupted header misses the beacon hash.
        let mut forged = p.clone();
        forged.header[0] ^= 0x01;
        assert_eq!(
            bind_to_beacon(&forged, &beacon),
            BindVerdict::HeaderMismatch
        );
        // Older than the cached head → stale, per the issue.
        beacon.height += 1;
        assert_eq!(bind_to_beacon(&p, &beacon), BindVerdict::StaleHeight);
        // Newer than any beacon heard → unverifiable, not accepted.
        beacon.height = p.head_height - 1;
        assert_eq!(bind_to_beacon(&p, &beacon), BindVerdict::AheadOfBeacon);
        // Wrong network beats everything.
        let mut other_net = beacon;
        other_net.height = p.head_height;
        other_net.network_id = 24;
        assert_eq!(bind_to_beacon(&p, &other_net), BindVerdict::WrongNetwork);
    }

    #[test]
    fn a_realistic_proof_rides_the_fragment_path_intact() {
        // A real proof exceeds one 256-byte BLE frame, so it must survive the proven G6
        // fragment split + reassembly byte-for-byte — same chunk class as the `0x36`
        // history response (crate::tx_history::HISTORY_FRAGMENT_CHUNK).
        let p = sample(320, 4096);
        let payload = encode_balance_proof(&p);
        assert!(payload.len() > 256, "the test must actually need fragments");
        let frags = fragment_message(
            MessageType::NimiqBalanceProof.to_u8(),
            &payload,
            crate::tx_history::HISTORY_FRAGMENT_CHUNK,
            [7u8; 8],
        );
        assert!(frags.len() > 1);
        let mut reasm = Reassembler::new();
        let mut out = None;
        for bytes in &frags {
            let f = crate::fragment::parse_fragment(bytes).expect("self-encoded fragment");
            if let Some(done) = reasm.accept(f, 1_000) {
                out = Some(done);
            }
        }
        let (orig_type, reassembled) = out.expect("all fragments delivered");
        assert_eq!(orig_type, MessageType::NimiqBalanceProof.to_u8());
        assert_eq!(decode_balance_proof(&reassembled), Some(p));
    }

    #[test]
    fn a_gateway_sourced_beacon_binds_a_proof_end_to_end() {
        // The whole chain this feature exists for: an RPC that serves the head hash → a
        // gateway beacon carrying it → the node's HeadCache retaining it → a proof whose
        // header hashes to it binds. Mock RPC, but every seam is the production one.
        use crate::beacon::HeadCache;
        use crate::gateway::{MeshGateway, RpcGateway};
        use crate::rpc::MockRpc;
        use std::sync::Arc;

        let header: Vec<u8> = (0u8..=255).cycle().take(300).collect();
        let rpc = Arc::new(MockRpc::new(4_600_000));
        rpc.set_head_hash(block_hash_of_header(&header));
        let gw = RpcGateway::new(rpc);

        let beacon = gw.head_beacon().expect("mock head is live");
        let mut cache = HeadCache::new(beacon.network_id);
        assert!(cache.accept(&beacon));

        let proof = BalanceProof {
            address: addr(0x99),
            head_height: 4_600_000,
            network_id: beacon.network_id,
            header,
            proof: vec![0xEE; 64],
        };
        assert_eq!(
            bind_to_beacon(&proof, &cache.latest_beacon().unwrap()),
            BindVerdict::Bound
        );
    }

    #[test]
    fn a_bound_real_layout_header_yields_its_state_root_and_nothing_less_does() {
        use crate::nimiq::header::{encode_micro_header, MicroHeader, VRF_SEED_LEN};

        let header = MicroHeader {
            network: 5,
            version: 2,
            block_number: 4_700_000,
            timestamp: 1_756_200_000_000,
            parent_hash: [0x10; 32],
            seed: [0x20; VRF_SEED_LEN],
            extra_data: vec![],
            state_root: [0x77; 32],
            body_root: [0x40; 32],
            diff_root: [0x50; 32],
            history_root: [0x60; 32],
        };
        let header_bytes = encode_micro_header(&header);
        let proof = BalanceProof {
            address: addr(0x01),
            head_height: header.block_number,
            network_id: header.network,
            header: header_bytes.clone(),
            proof: vec![0xAB; 32],
        };
        let mut beacon = HeadBeacon::new(header.block_number, header.network);
        beacon.block_hash = block_hash_of_header(&header_bytes);

        // The full step-1..3 chain: bound, parsed, cross-checked → the root.
        assert_eq!(
            state_root_for_bound_proof(&proof, &beacon),
            Some([0x77; 32])
        );

        // Unbound (wrong hash) → no root, even though the header itself parses.
        let mut cold = beacon;
        cold.block_hash = [0x0F; 32];
        assert_eq!(state_root_for_bound_proof(&proof, &cold), None);

        // Bound hash but a header whose own fields disagree with the envelope → reject.
        let mut lying = proof.clone();
        lying.head_height += 1;
        let mut moved = beacon;
        moved.height += 1;
        assert_eq!(state_root_for_bound_proof(&lying, &moved), None);

        // A bound blob that is not a real micro-header layout → fail closed.
        let junk: Vec<u8> = vec![0xFF; 300];
        let mut junk_beacon = HeadBeacon::new(proof.head_height, proof.network_id);
        junk_beacon.block_hash = block_hash_of_header(&junk);
        let mut junk_proof = proof.clone();
        junk_proof.header = junk;
        assert_eq!(state_root_for_bound_proof(&junk_proof, &junk_beacon), None);
    }

    #[test]
    fn the_type_byte_roundtrips_through_the_registry() {
        assert_eq!(MessageType::NimiqBalanceProof.to_u8(), 0x37);
        assert_eq!(
            MessageType::from_u8(0x37),
            Some(MessageType::NimiqBalanceProof)
        );
    }
}
