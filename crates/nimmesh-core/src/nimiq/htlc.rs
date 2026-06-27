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
use super::tx::PUBLIC_KEY_LEN;

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
    /// The contract timeout as a **block height** (u64). After this height the sender may
    /// refund; before it, the recipient may claim with the preimage. (The mesh head-beacon
    /// is the clock — see `docs/swap/SWAP.md` on the timelock ladder.)
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
}
