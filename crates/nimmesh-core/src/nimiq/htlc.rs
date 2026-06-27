//! # nimiq::htlc — byte-exact Albatross **HTLC** transaction serialization (mesh swap F1)
//!
//! Extends the [`crate::nimiq::tx`] basic-transfer serializer to the **HTLC** account type,
//! the on-chain primitive a cross-chain atomic swap is built on (`docs/swap/SWAP.md`). All
//! layouts are asserted byte-for-byte against `@nimiq/core` 2.7.0 fixtures
//! (`tests/fixtures/swap_htlc_fixtures.json`, generator `scripts/fixtures/gen-htlc-fixtures.mjs`;
//! the exact byte layout was spiked in `docs/swap/F0-HTLC-FINDINGS.md`).
//!
//! Two transactions make up the NIM leg of a swap:
//! - **Creation (funding)** — an *extended-format* tx that locks `value` luna into a new HTLC
//!   contract whose unlock conditions are the 82-byte [`HtlcCreationData`]. Built + **signed**
//!   here; proven `ACCEPTED` by `@nimiq/core`'s own validator (`scripts/fixtures/feasibility-test.mjs`).
//! - **Redeem (claim / refund)** — an extended-format tx *from* the contract. Its
//!   [`serialize_content`](RedeemKind) (the signing payload) is built byte-exact here; the
//!   resolve **proof** (carrying the preimage) is a follow-up gated against `core-rs-albatross`
//!   / a live testnet broadcast, because `@nimiq/core` JS deliberately cannot sign or verify
//!   HTLC redemptions (see `docs/swap/F0-HTLC-FINDINGS.md`).
//!
//! Pure bytes — no key material, no network (the seed never enters this module; signing is the
//! caller's [`crate::nimiq::signer`] seam). Testnet-by-default; mainnet is gated.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use super::address::{Address, ADDRESS_LEN};
use super::tx::{signature_proof_single_sig, PUBLIC_KEY_LEN, SIGNATURE_LEN};

/// `TransactionFormat::Extended` discriminant (leading byte of the full wire blob). Unlike the
/// 139-byte `Basic` format, an HTLC tx carries contract data and/or a non-basic account type,
/// so it is always the extended format.
const FORMAT_EXTENDED: u8 = 0x01;
/// `AccountType::Basic` discriminant.
const ACCOUNT_TYPE_BASIC: u8 = 0x00;
/// `AccountType::HTLC` discriminant (the contract type).
pub const ACCOUNT_TYPE_HTLC: u8 = 0x02;
/// `TransactionFlags::CONTRACT_CREATION` — set on the funding tx that mints the HTLC.
const FLAG_CONTRACT_CREATION: u8 = 0b0000_0001;
/// No flags — a redeem tx is an ordinary outgoing transfer from the contract.
const FLAG_NONE: u8 = 0b0000_0000;
/// The fixed length of the serialized [`HtlcCreationData`].
pub const HTLC_DATA_LEN: usize = ADDRESS_LEN + ADDRESS_LEN + 1 + 32 + 1 + 8; // = 82
/// Length of a raw hash root / preimage (Blake2b-256 / SHA-256 are both 32 bytes).
pub const HASH_LEN: usize = 32;

/// The hash function an HTLC's hashlock uses. The discriminant is the on-wire `hashAlgorithm`
/// byte inside [`HtlcCreationData`]. **SHA-256 is the cross-chain choice** (Bitcoin's HTLC
/// script hashes with SHA-256, so a NIM↔BTC swap must share a SHA-256 hashlock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HashAlgorithm {
    /// Nimiq's native hash. Cheapest on the NIM side, but not BTC-compatible.
    Blake2b = 1,
    /// SHA-256 — shared with Bitcoin/most chains; use this for a cross-chain swap.
    Sha256 = 3,
}

/// The 82-byte HTLC creation data — the contract's unlock conditions, carried as the
/// `recipient_data` of the funding tx.
///
/// Layout (big-endian): `sender(20) || recipient(20) || hash_algorithm(1) || hash_root(32) ||
/// hash_count(1) || timeout(8, u64 block height)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtlcCreationData {
    /// The HTLC creator / refunder — reclaims the funds via the timeout path after `timeout`.
    pub htlc_sender: Address,
    /// The claimant — takes the funds before `timeout` by revealing the preimage of `hash_root`.
    pub htlc_recipient: Address,
    /// The hashlock's hash function (use [`HashAlgorithm::Sha256`] for cross-chain).
    pub hash_algorithm: HashAlgorithm,
    /// `H` — the hashlock. For `hash_count == 1`, `H = hash_algorithm(preimage)`.
    pub hash_root: [u8; HASH_LEN],
    /// How many times the preimage is hashed to reach `hash_root` (1 for a simple swap).
    pub hash_count: u8,
    /// The contract timeout as a **Unix timestamp in MILLISECONDS** (u64) — compared by the
    /// validator against the block's `block_state.time`, **NOT** the block height
    /// (`core-rs-albatross` htlc_contract.rs; **proven live on testnet**: a height-valued timeout
    /// reads as expired-at-birth, so the claim path is rejected and only refund works). After this
    /// time the sender may refund (`TimeoutResolve`); before it, the recipient may claim with the
    /// preimage (`RegularTransfer`).
    pub timeout: u64,
}

impl HtlcCreationData {
    /// Serialize to the canonical 82 bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HTLC_DATA_LEN);
        out.extend_from_slice(self.htlc_sender.as_bytes());
        out.extend_from_slice(self.htlc_recipient.as_bytes());
        out.push(self.hash_algorithm as u8);
        out.extend_from_slice(&self.hash_root);
        out.push(self.hash_count);
        out.extend_from_slice(&self.timeout.to_be_bytes());
        debug_assert_eq!(out.len(), HTLC_DATA_LEN);
        out
    }
}

/// `Blake2b-256` of `bytes`.
fn blake2b256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Append a Nimiq var-length byte field: a length prefix then the bytes. Lengths < 128 are a
/// single byte (every field in the funding tx + redeem content); larger lengths use the
/// LEB128 continuation form (needed only by the redeem proof, built later).
fn push_var_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut len = bytes.len();
    loop {
        let mut byte = (len & 0x7f) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(bytes);
}

/// The shared **`serializeContent`** signing payload for an extended-format tx. Identical shape
/// to a basic transfer's 67-byte content, generalized over account types, flags, and the
/// front-loaded `recipient_data` (the HTLC data on a funding tx; empty on a redeem).
///
/// Layout: `recipient_data_len(2) || recipient_data || sender(20) || sender_type(1) ||
/// recipient(20) || recipient_type(1) || value(8) || fee(8) || vsh(4) || network(1) ||
/// flags(1) || sender_data_len(1)=0`. (`sender_data` is always empty in our swaps.)
#[allow(clippy::too_many_arguments)]
fn extended_content(
    recipient_data: &[u8],
    sender: &Address,
    sender_type: u8,
    recipient: &Address,
    recipient_type: u8,
    value: u64,
    fee: u64,
    validity_start_height: u32,
    network_id: u8,
    flags: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + recipient_data.len() + 75);
    out.extend_from_slice(&(recipient_data.len() as u16).to_be_bytes());
    out.extend_from_slice(recipient_data);
    out.extend_from_slice(sender.as_bytes());
    out.push(sender_type);
    out.extend_from_slice(recipient.as_bytes());
    out.push(recipient_type);
    out.extend_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&fee.to_be_bytes());
    out.extend_from_slice(&validity_start_height.to_be_bytes());
    out.push(network_id);
    out.push(flags);
    out.push(0x00); // sender_data length (always empty here)
    out
}

/// A Nimiq HTLC **creation (funding)** transaction: locks `value` luna into a new HTLC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtlcCreation {
    /// The funding account (a basic account) paying into the contract.
    pub funder: Address,
    /// The contract's unlock conditions.
    pub data: HtlcCreationData,
    /// Amount locked into the HTLC, in luna.
    pub value: u64,
    /// Fee in luna.
    pub fee: u64,
    /// `validityStartHeight` — anchor to the latest known head.
    pub validity_start_height: u32,
    /// Albatross network-id byte (5 = testnet).
    pub network_id: u8,
}

impl HtlcCreation {
    /// The deterministic **contract address** the funded HTLC will live at:
    /// `Blake2b-256(serializeContent with the recipient field zeroed)[..20]` (the recipient of
    /// a creation tx must equal this address, so it is computed with a zero placeholder first).
    pub fn contract_address(&self) -> Address {
        let zero = Address::from_bytes([0u8; ADDRESS_LEN]);
        let content = extended_content(
            &self.data.serialize(),
            &self.funder,
            ACCOUNT_TYPE_BASIC,
            &zero,
            ACCOUNT_TYPE_HTLC,
            self.value,
            self.fee,
            self.validity_start_height,
            self.network_id,
            FLAG_CONTRACT_CREATION,
        );
        let hash = blake2b256(&content);
        let mut addr = [0u8; ADDRESS_LEN];
        addr.copy_from_slice(&hash[..ADDRESS_LEN]);
        Address::from_bytes(addr)
    }

    /// The `serializeContent` signing payload (the creator signs this).
    pub fn serialize_content(&self) -> Vec<u8> {
        extended_content(
            &self.data.serialize(),
            &self.funder,
            ACCOUNT_TYPE_BASIC,
            &self.contract_address(),
            ACCOUNT_TYPE_HTLC,
            self.value,
            self.fee,
            self.validity_start_height,
            self.network_id,
            FLAG_CONTRACT_CREATION,
        )
    }

    /// The canonical transaction hash: `Blake2b-256(serializeContent)`.
    pub fn tx_hash(&self) -> [u8; 32] {
        blake2b256(&self.serialize_content())
    }

    /// The full extended-format wire blob given a `proof` (empty for the unsigned form; the
    /// 98-byte single-sig proof for the signed, broadcast-ready form).
    ///
    /// Layout: `format(1)=1 || sender(20) || sender_type(1) || varint(sender_data)=0 ||
    /// recipient(20) || recipient_type(1) || varint(recipient_data) || recipient_data ||
    /// value(8) || fee(8) || vsh(4) || network(1) || flags(1) || varint(proof) || proof`.
    pub fn serialize_wire(&self, proof: &[u8]) -> Vec<u8> {
        let data = self.data.serialize();
        let mut out = Vec::with_capacity(1 + 44 + data.len() + 22 + proof.len());
        out.push(FORMAT_EXTENDED);
        out.extend_from_slice(self.funder.as_bytes());
        out.push(ACCOUNT_TYPE_BASIC);
        push_var_bytes(&mut out, &[]); // empty sender_data
        out.extend_from_slice(self.contract_address().as_bytes());
        out.push(ACCOUNT_TYPE_HTLC);
        push_var_bytes(&mut out, &data); // recipient_data = the HTLC creation data
        out.extend_from_slice(&self.value.to_be_bytes());
        out.extend_from_slice(&self.fee.to_be_bytes());
        out.extend_from_slice(&self.validity_start_height.to_be_bytes());
        out.push(self.network_id);
        out.push(FLAG_CONTRACT_CREATION);
        push_var_bytes(&mut out, proof);
        out
    }
}

/// A Nimiq HTLC **redeem** transaction — an outgoing transfer *from* a funded HTLC contract.
/// This builds the `serializeContent` signing payload byte-exact; the resolve **proof** (which
/// carries the preimage for a claim, or just the sender's signature for a timeout refund) is a
/// follow-up gated against `core-rs-albatross` (see module docs / F0 findings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtlcRedeem {
    /// The HTLC contract address the funds leave from.
    pub contract: Address,
    /// Where the funds go (the claimant on a claim; the original funder on a refund).
    pub recipient: Address,
    /// Amount moved out of the contract, in luna.
    pub value: u64,
    /// Fee in luna.
    pub fee: u64,
    /// `validityStartHeight`.
    pub validity_start_height: u32,
    /// Network-id byte.
    pub network_id: u8,
}

impl HtlcRedeem {
    /// The `serializeContent` signing payload: a basic-shaped content whose **sender type is
    /// HTLC** and whose sender is the contract address.
    pub fn serialize_content(&self) -> Vec<u8> {
        extended_content(
            &[], // no recipient data on a redeem
            &self.contract,
            ACCOUNT_TYPE_HTLC,
            &self.recipient,
            ACCOUNT_TYPE_BASIC,
            self.value,
            self.fee,
            self.validity_start_height,
            self.network_id,
            FLAG_NONE,
        )
    }

    /// The canonical transaction hash: `Blake2b-256(serializeContent)`.
    pub fn tx_hash(&self) -> [u8; 32] {
        blake2b256(&self.serialize_content())
    }

    /// The full extended-format redeem wire: an outgoing transfer **from** the HTLC contract,
    /// carrying `proof` (a [`regular_transfer_proof`] to claim, or a [`timeout_resolve_proof`] to
    /// refund). Layout: `format(1)=1 || sender(20)=contract || sender_type(1)=HTLC ||
    /// varint(sender_data)=0 || recipient(20) || recipient_type(1)=basic ||
    /// varint(recipient_data)=0 || value(8) || fee(8) || vsh(4) || network(1) || flags(1)=0 ||
    /// varint(proof) || proof`.
    pub fn serialize_wire(&self, proof: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 44 + 22 + proof.len());
        out.push(FORMAT_EXTENDED);
        out.extend_from_slice(self.contract.as_bytes());
        out.push(ACCOUNT_TYPE_HTLC);
        push_var_bytes(&mut out, &[]); // empty sender_data
        out.extend_from_slice(self.recipient.as_bytes());
        out.push(ACCOUNT_TYPE_BASIC);
        push_var_bytes(&mut out, &[]); // empty recipient_data (basic recipient)
        out.extend_from_slice(&self.value.to_be_bytes());
        out.extend_from_slice(&self.fee.to_be_bytes());
        out.extend_from_slice(&self.validity_start_height.to_be_bytes());
        out.push(self.network_id);
        out.push(FLAG_NONE);
        push_var_bytes(&mut out, proof);
        out
    }
}

/// Decode an [`HtlcCreation`] back from its funding wire (the inverse of
/// [`HtlcCreation::serialize_wire`], ignoring the trailing proof). Returns `None` for any wire
/// that is not a well-formed empty-sender-data, 82-byte-HTLC-data creation tx. Lets a claimant /
/// refunder rebuild the contract context (address, recipient, hashlock, value) from the funding
/// tx it observed on the mesh.
pub fn decode_creation_wire(wire: &[u8]) -> Option<HtlcCreation> {
    // Fixed layout (sender_data len 0, recipient_data len 82 → both single-byte varints):
    // format(1) sender(20) sender_type(1) sd_len(1)=0 recipient(20) recipient_type(1)=2
    //   rd_len(1)=0x52 htlc_data(82) value(8) fee(8) vsh(4) network(1) flags(1)=1 ...
    const HEAD: usize = 1 + ADDRESS_LEN + 1 + 1 + ADDRESS_LEN + 1 + 1; // 45
    if wire.len() < HEAD + HTLC_DATA_LEN + 8 + 8 + 4 + 1 + 1
        || wire[0] != FORMAT_EXTENDED
        || wire[ADDRESS_LEN + 1] != ACCOUNT_TYPE_BASIC
        || wire[ADDRESS_LEN + 2] != 0 // empty sender_data
        || wire[2 + 2 * ADDRESS_LEN + 1] != ACCOUNT_TYPE_HTLC
        || wire[2 + 2 * ADDRESS_LEN + 2] != HTLC_DATA_LEN as u8
    {
        return None;
    }
    let mut funder = [0u8; ADDRESS_LEN];
    funder.copy_from_slice(&wire[1..1 + ADDRESS_LEN]);
    let data = &wire[HEAD..HEAD + HTLC_DATA_LEN];
    let hash_algorithm = match data[2 * ADDRESS_LEN] {
        1 => HashAlgorithm::Blake2b,
        3 => HashAlgorithm::Sha256,
        _ => return None,
    };
    let mut htlc_sender = [0u8; ADDRESS_LEN];
    htlc_sender.copy_from_slice(&data[..ADDRESS_LEN]);
    let mut htlc_recipient = [0u8; ADDRESS_LEN];
    htlc_recipient.copy_from_slice(&data[ADDRESS_LEN..2 * ADDRESS_LEN]);
    let mut hash_root = [0u8; HASH_LEN];
    hash_root.copy_from_slice(&data[2 * ADDRESS_LEN + 1..2 * ADDRESS_LEN + 1 + HASH_LEN]);
    let hash_count = data[2 * ADDRESS_LEN + 1 + HASH_LEN];
    let timeout = u64::from_be_bytes(data[HTLC_DATA_LEN - 8..].try_into().ok()?);
    let mut o = HEAD + HTLC_DATA_LEN;
    let value = u64::from_be_bytes(wire[o..o + 8].try_into().ok()?);
    o += 8;
    let fee = u64::from_be_bytes(wire[o..o + 8].try_into().ok()?);
    o += 8;
    let validity_start_height = u32::from_be_bytes(wire[o..o + 4].try_into().ok()?);
    o += 4;
    let network_id = wire[o];
    Some(HtlcCreation {
        funder: Address::from_bytes(funder),
        data: HtlcCreationData {
            htlc_sender: Address::from_bytes(htlc_sender),
            htlc_recipient: Address::from_bytes(htlc_recipient),
            hash_algorithm,
            hash_root,
            hash_count,
            timeout,
        },
        value,
        fee,
        validity_start_height,
        network_id,
    })
}

/// `OutgoingHTLCTransactionProof` variant byte: **RegularTransfer** (claim with preimage). PoS
/// variant ids start at 0 (`core-rs-albatross`).
pub const PROOF_REGULAR_TRANSFER: u8 = 0x00;
/// `OutgoingHTLCTransactionProof` variant byte: **TimeoutResolve** (refund after timeout).
pub const PROOF_TIMEOUT_RESOLVE: u8 = 0x02;
/// `PreImage::PreImage32` discriminant — the custom serializer writes the **length** (32), not an
/// index (`core-rs-albatross` `impl Serialize for PreImage`).
const PRE_IMAGE_32_TAG: u8 = 32;

/// The **claim-with-preimage** (`RegularTransfer`) resolve proof. Revealing this on-chain reveals
/// the preimage. Layout: `0x00 (variant) || hash_depth(1) || hash_root:AnyHash(algo||32) ||
/// pre_image:PreImage(0x20||32) || signature_proof(98)` — built byte-for-byte to the
/// `core-rs-albatross` encoding (the gate is a live testnet redeem, since `@nimiq/core` JS can't).
pub fn regular_transfer_proof(
    hash_algorithm: HashAlgorithm,
    hash_root: &[u8; HASH_LEN],
    pre_image: &[u8; HASH_LEN],
    public_key: &[u8; PUBLIC_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 1 + 1 + HASH_LEN + 1 + HASH_LEN + SIGNATURE_PROOF_LEN);
    out.push(PROOF_REGULAR_TRANSFER);
    out.push(1); // hash_depth: present the preimage once (hash_count = 1)
    out.push(hash_algorithm as u8); // AnyHash tag (sha256 = 3)
    out.extend_from_slice(hash_root);
    out.push(PRE_IMAGE_32_TAG); // PreImage32 tag (= 32)
    out.extend_from_slice(pre_image);
    out.extend_from_slice(&signature_proof_single_sig(public_key, signature));
    out
}

/// The **timeout-refund** (`TimeoutResolve`) resolve proof, signed by the HTLC creator/sender.
/// Layout: `0x02 (variant) || signature_proof(98)`.
pub fn timeout_resolve_proof(
    public_key: &[u8; PUBLIC_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + SIGNATURE_PROOF_LEN);
    out.push(PROOF_TIMEOUT_RESOLVE);
    out.extend_from_slice(&signature_proof_single_sig(public_key, signature));
    out
}

/// Length of the standard single-sig `SignatureProof` blob a signed creation tx carries.
pub const SIGNATURE_PROOF_LEN: usize = 1 + PUBLIC_KEY_LEN + 1 + 64; // = 98

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};

    fn addr(hex: &str) -> Address {
        let mut b = [0u8; ADDRESS_LEN];
        b.copy_from_slice(&hex_to_bytes(hex).unwrap());
        Address::from_bytes(b)
    }
    fn root(hex: &str) -> [u8; 32] {
        let mut b = [0u8; 32];
        b.copy_from_slice(&hex_to_bytes(hex).unwrap());
        b
    }

    // Fixture `sha256-1nim-t12345` from tests/fixtures/swap_htlc_fixtures.json (@nimiq/core 2.7.0).
    fn fixture_creation() -> HtlcCreation {
        HtlcCreation {
            funder: addr("4d4dbe917544b07922348a66b9c4b5a5a5f34a9f"),
            data: HtlcCreationData {
                htlc_sender: addr("4d4dbe917544b07922348a66b9c4b5a5a5f34a9f"),
                htlc_recipient: addr("567866611c1a2c85a1a676bb8c845d0655fea1a6"),
                hash_algorithm: HashAlgorithm::Sha256,
                hash_root: root("ddaa3357f98f1e5186c4317959832aeb4f616ef44b38bb7279ec89836b2873e5"),
                hash_count: 1,
                timeout: 12345,
            },
            value: 100000,
            fee: 0,
            validity_start_height: 100,
            network_id: 5,
        }
    }

    #[test]
    fn htlc_data_is_byte_exact() {
        assert_eq!(
            bytes_to_hex(&fixture_creation().data.serialize()),
            "4d4dbe917544b07922348a66b9c4b5a5a5f34a9f567866611c1a2c85a1a676bb8c845d0655fea1a603ddaa3357f98f1e5186c4317959832aeb4f616ef44b38bb7279ec89836b2873e5010000000000003039"
        );
        assert_eq!(fixture_creation().data.serialize().len(), HTLC_DATA_LEN);
    }

    #[test]
    fn contract_address_matches_nimiq_core() {
        assert_eq!(
            fixture_creation().contract_address().to_hex(),
            "41c0fb47c6af37950dcbbbe56b7a7a489d3001cf"
        );
    }

    #[test]
    fn creation_content_and_hash_are_byte_exact() {
        let c = fixture_creation();
        assert_eq!(
            bytes_to_hex(&c.serialize_content()),
            "00524d4dbe917544b07922348a66b9c4b5a5a5f34a9f567866611c1a2c85a1a676bb8c845d0655fea1a603ddaa3357f98f1e5186c4317959832aeb4f616ef44b38bb7279ec89836b2873e50100000000000030394d4dbe917544b07922348a66b9c4b5a5a5f34a9f0041c0fb47c6af37950dcbbbe56b7a7a489d3001cf0200000000000186a0000000000000000000000064050100"
        );
        assert_eq!(
            bytes_to_hex(&c.tx_hash()),
            "22061f885e21ab1aa392a0010a853d6d2a8cd03cedc4db4c27b19d8191bf2335"
        );
    }

    #[test]
    fn creation_unsigned_wire_is_byte_exact() {
        // The fixture rawHex is the UNSIGNED extended wire (proof length 0).
        assert_eq!(
            bytes_to_hex(&fixture_creation().serialize_wire(&[])),
            "014d4dbe917544b07922348a66b9c4b5a5a5f34a9f000041c0fb47c6af37950dcbbbe56b7a7a489d3001cf02524d4dbe917544b07922348a66b9c4b5a5a5f34a9f567866611c1a2c85a1a676bb8c845d0655fea1a603ddaa3357f98f1e5186c4317959832aeb4f616ef44b38bb7279ec89836b2873e501000000000000303900000000000186a0000000000000000000000064050100"
        );
    }

    #[test]
    fn signed_creation_wire_has_the_expected_shape() {
        // A signed funding tx appends the 98-byte single-sig proof -> 248 bytes total, matching
        // the @nimiq/core feasibility test (scripts/fixtures/feasibility-test.mjs, "ACCEPTED").
        let proof = vec![0u8; SIGNATURE_PROOF_LEN];
        let wire = fixture_creation().serialize_wire(&proof);
        assert_eq!(wire.len(), 248);
        assert_eq!(wire[0], FORMAT_EXTENDED);
        // proof length prefix (0x62 = 98) precedes the proof at the tail.
        assert_eq!(wire[wire.len() - SIGNATURE_PROOF_LEN - 1], 0x62);
    }

    #[test]
    fn redeem_content_is_byte_exact() {
        // The claim/refund tx FROM the contract: same content shape, sender_type = HTLC.
        let redeem = HtlcRedeem {
            contract: addr("41c0fb47c6af37950dcbbbe56b7a7a489d3001cf"),
            recipient: addr("567866611c1a2c85a1a676bb8c845d0655fea1a6"),
            value: 100000,
            fee: 0,
            validity_start_height: 101,
            network_id: 5,
        };
        assert_eq!(
            bytes_to_hex(&redeem.serialize_content()),
            "000041c0fb47c6af37950dcbbbe56b7a7a489d3001cf02567866611c1a2c85a1a676bb8c845d0655fea1a60000000000000186a0000000000000000000000065050000"
        );
    }

    #[test]
    fn regular_transfer_proof_has_the_core_rs_albatross_shape() {
        // 0x00 || hash_depth(1) || AnyHash(0x03||32) || PreImage(0x20||32) || sigProof(98) = 166 B.
        let proof = regular_transfer_proof(
            HashAlgorithm::Sha256,
            &[0xAA; 32],
            &[0xBB; 32],
            &[0xCC; 32],
            &[0xDD; 64],
        );
        assert_eq!(proof.len(), 1 + 1 + 1 + 32 + 1 + 32 + SIGNATURE_PROOF_LEN);
        assert_eq!(proof.len(), 166);
        assert_eq!(proof[0], PROOF_REGULAR_TRANSFER); // 0x00
        assert_eq!(proof[1], 1); // hash_depth
        assert_eq!(proof[2], 0x03); // AnyHash::Sha256 tag
        assert_eq!(&proof[3..35], &[0xAA; 32]); // hash_root
        assert_eq!(proof[35], PRE_IMAGE_32_TAG); // 0x20 (= 32)
        assert_eq!(&proof[36..68], &[0xBB; 32]); // pre_image
        assert_eq!(proof[68], 0x00); // SignatureProof type byte (Ed25519)
    }

    #[test]
    fn timeout_resolve_proof_is_variant_then_sigproof() {
        let proof = timeout_resolve_proof(&[1; 32], &[2; 64]);
        assert_eq!(proof.len(), 1 + SIGNATURE_PROOF_LEN); // 99
        assert_eq!(proof[0], PROOF_TIMEOUT_RESOLVE); // 0x02
    }

    #[test]
    fn redeem_wire_is_leb128_length_prefixed() {
        // The 166-byte RegularTransfer proof rides the wire as `a6 01 || <166 bytes>` (LEB128).
        let redeem = HtlcRedeem {
            contract: addr("41c0fb47c6af37950dcbbbe56b7a7a489d3001cf"),
            recipient: addr("567866611c1a2c85a1a676bb8c845d0655fea1a6"),
            value: 100000,
            fee: 0,
            validity_start_height: 101,
            network_id: 5,
        };
        let proof = regular_transfer_proof(
            HashAlgorithm::Sha256,
            &[0xAA; 32],
            &[0xBB; 32],
            &[0xCC; 32],
            &[0xDD; 64],
        );
        let wire = redeem.serialize_wire(&proof);
        // The trailing bytes are the LEB128 length (166 = a6 01) then the 166-byte proof.
        let tail = &wire[wire.len() - 2 - 166..];
        assert_eq!(tail[0], 0xA6);
        assert_eq!(tail[1], 0x01);
        assert_eq!(&tail[2..], &proof[..]);
        assert_eq!(wire[0], FORMAT_EXTENDED);
    }
}
