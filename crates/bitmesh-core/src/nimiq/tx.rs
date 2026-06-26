//! # nimiq::tx — byte-exact Albatross **basic transfer** serialization
//!
//! Two deterministic serializations of a single-signer NIM transfer, matching
//! `@nimiq/core` (v2.7.0) byte-for-byte (asserted against committed fixtures):
//!
//! - [`Transfer::serialize_content`] — the **67-byte signing payload** (`serializeContent`)
//!   the Ed25519 signature is computed over, and whose `Blake2b-256` is the canonical
//!   [`Transfer::tx_hash`] (`tx.hash()`).
//! - [`Transfer::serialize_basic`] — the full **139-byte** `Basic`-format wire blob
//!   (`tx.serialize()`): `format || proof_type || pubkey || recipient || value || fee ||
//!   vsh || network || signature`. This is the self-contained, self-authenticating blob
//!   that rides the mesh (GOAL.md "~139-byte blob").
//!
//! Pure bytes — no key material, no network. Fee is always 0 (the basic format has no fee
//! field beyond the content) and data is empty, matching the G3 transfer shape.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use super::address::Address;

/// `AccountType::Basic` discriminant — the only sender/recipient type G3 builds.
const ACCOUNT_TYPE_BASIC: u8 = 0x00;
/// `TransactionFormat::Basic` discriminant (leading byte of the full wire blob).
const FORMAT_BASIC: u8 = 0x00;
/// `SignatureProof` type byte for a plain Ed25519 single-sig (no flags) on the wire blob.
const PROOF_TYPE_ED25519_SINGLE: u8 = 0x00;
/// Empty `flags` byte in `serializeContent` (no contract-creation / signalling).
const FLAGS_NONE: u8 = 0x00;
/// Empty `sender_data` length byte in `serializeContent`.
const SENDER_DATA_LEN_NONE: u8 = 0x00;

/// The exact byte length of `serializeContent` for an empty-data basic transfer.
pub const CONTENT_LEN: usize = 67;
/// The exact byte length of the full `Basic`-format serialized transfer.
pub const BASIC_WIRE_LEN: usize = 139;
/// A raw Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;
/// A raw Ed25519 public-key length.
pub const PUBLIC_KEY_LEN: usize = 32;

/// A single-signer Nimiq basic transfer (no data, fee 0). Pure value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Sender account address (= `Blake2b-256(pubkey)[..20]` for a basic tx).
    pub sender: Address,
    /// Recipient account address.
    pub recipient: Address,
    /// Amount in luna (1 NIM = 100_000 luna).
    pub value: u64,
    /// Fee in luna. Always 0 for the G3 basic transfer.
    pub fee: u64,
    /// `validityStartHeight` — anchor to the latest known head (RISKS.md #1).
    pub validity_start_height: u32,
    /// Albatross network-id byte (5 = testnet, the G3 default).
    pub network_id: u8,
}

impl Transfer {
    /// The 67-byte `serializeContent` signing payload.
    ///
    /// Layout (all integers big-endian):
    /// `data_len:u16=0 || sender:20 || sender_type:u8=0 || recipient:20 || recipient_type:u8=0
    ///  || value:u64 || fee:u64 || vsh:u32 || network:u8 || flags:u8=0 || sender_data_len:u8=0`.
    pub fn serialize_content(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CONTENT_LEN);
        out.extend_from_slice(&0u16.to_be_bytes()); // data length (empty)
                                                    // (no data bytes)
        out.extend_from_slice(self.sender.as_bytes());
        out.push(ACCOUNT_TYPE_BASIC);
        out.extend_from_slice(self.recipient.as_bytes());
        out.push(ACCOUNT_TYPE_BASIC);
        out.extend_from_slice(&self.value.to_be_bytes());
        out.extend_from_slice(&self.fee.to_be_bytes());
        out.extend_from_slice(&self.validity_start_height.to_be_bytes());
        out.push(self.network_id);
        out.push(FLAGS_NONE);
        out.push(SENDER_DATA_LEN_NONE);
        debug_assert_eq!(out.len(), CONTENT_LEN);
        out
    }

    /// The canonical transaction hash: `Blake2b-256(serializeContent)`.
    pub fn tx_hash(&self) -> [u8; 32] {
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(self.serialize_content());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    /// The full 139-byte `Basic`-format wire blob, given the signer's public key and the
    /// Ed25519 signature over [`Transfer::serialize_content`].
    ///
    /// Layout: `format:u8=0 || proof_type:u8=0 || pubkey:32 || recipient:20 || value:u64 ||
    /// fee:u64 || vsh:u32 || network:u8 || signature:64`. (The sender address is implicit
    /// in the basic format — it is derived from the embedded public key.)
    pub fn serialize_basic(
        &self,
        public_key: &[u8; PUBLIC_KEY_LEN],
        signature: &[u8; SIGNATURE_LEN],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(BASIC_WIRE_LEN);
        out.push(FORMAT_BASIC);
        out.push(PROOF_TYPE_ED25519_SINGLE);
        out.extend_from_slice(public_key);
        out.extend_from_slice(self.recipient.as_bytes());
        out.extend_from_slice(&self.value.to_be_bytes());
        out.extend_from_slice(&self.fee.to_be_bytes());
        out.extend_from_slice(&self.validity_start_height.to_be_bytes());
        out.push(self.network_id);
        out.extend_from_slice(signature);
        debug_assert_eq!(out.len(), BASIC_WIRE_LEN);
        out
    }
}

/// Build the 98-byte single-sig `SignatureProof` blob: `type:u8=0 || pubkey:32 ||
/// merkle_path_len:u8=0 || signature:64`. (The basic-format wire blob embeds the pubkey +
/// signature directly instead; this standalone proof is what an *extended* tx or an
/// external verifier consumes, and is asserted byte-exact against the fixtures.)
pub fn signature_proof_single_sig(
    public_key: &[u8; PUBLIC_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + PUBLIC_KEY_LEN + 1 + SIGNATURE_LEN);
    out.push(PROOF_TYPE_ED25519_SINGLE);
    out.extend_from_slice(public_key);
    out.push(0x00); // empty merkle path (single signer)
    out.extend_from_slice(signature);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};

    fn addr(hex: &str) -> Address {
        let mut b = [0u8; 20];
        b.copy_from_slice(&hex_to_bytes(hex).unwrap());
        Address::from_bytes(b)
    }

    #[test]
    fn content_matches_reference_bytes_and_len() {
        let t = Transfer {
            sender: addr("4d4dbe917544b07922348a66b9c4b5a5a5f34a9f"),
            recipient: addr("567866611c1a2c85a1a676bb8c845d0655fea1a6"),
            value: 100000,
            fee: 0,
            validity_start_height: 1234,
            network_id: 5,
        };
        let content = t.serialize_content();
        assert_eq!(content.len(), CONTENT_LEN);
        assert_eq!(
            bytes_to_hex(&content),
            "00004d4dbe917544b07922348a66b9c4b5a5a5f34a9f00567866611c1a2c85a1a676bb8c845d0655fea1a60000000000000186a00000000000000000000004d2050000"
        );
        assert_eq!(
            bytes_to_hex(&t.tx_hash()),
            "4df874dac3672974f31bc64d022d89ee8d744ac72ebc6be263663561fb479274"
        );
    }
}
