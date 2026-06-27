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
use ed25519_dalek::{Signature, VerifyingKey};

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

/// A basic transfer recovered from its wire blob, with the embedded public key + signature.
/// The product of [`decode_basic_wire`]; the sender is derived from the public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedTransfer {
    /// The reconstructed transfer (sender derived from `public_key`).
    pub transfer: Transfer,
    /// The signer's Ed25519 public key (32 bytes), embedded in the basic wire.
    pub public_key: [u8; PUBLIC_KEY_LEN],
    /// The Ed25519 signature (64 bytes) over `serializeContent`.
    pub signature: [u8; SIGNATURE_LEN],
}

/// Decode a 139-byte `Basic`-format wire blob back into its fields (the inverse of
/// [`Transfer::serialize_basic`]). Returns `None` for any wrong length, non-`Basic` format,
/// or non-single-Ed25519 proof type. Pure, panic-free, no key material. The sender address
/// is derived from the embedded public key (`Blake2b-256(pubkey)[..20]`).
pub fn decode_basic_wire(wire: &[u8]) -> Option<DecodedTransfer> {
    if wire.len() != BASIC_WIRE_LEN
        || wire[0] != FORMAT_BASIC
        || wire[1] != PROOF_TYPE_ED25519_SINGLE
    {
        return None;
    }
    let mut public_key = [0u8; PUBLIC_KEY_LEN];
    public_key.copy_from_slice(&wire[2..34]);
    let mut recipient = [0u8; 20];
    recipient.copy_from_slice(&wire[34..54]);
    let value = u64::from_be_bytes(wire[54..62].try_into().ok()?);
    let fee = u64::from_be_bytes(wire[62..70].try_into().ok()?);
    let validity_start_height = u32::from_be_bytes(wire[70..74].try_into().ok()?);
    let network_id = wire[74];
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&wire[75..139]);

    let transfer = Transfer {
        sender: Address::from_public_key(&public_key),
        recipient: Address::from_bytes(recipient),
        value,
        fee,
        validity_start_height,
        network_id,
    };
    Some(DecodedTransfer {
        transfer,
        public_key,
        signature,
    })
}

/// Verify a 139-byte basic-transfer wire blob is a **well-formed, correctly-signed** Nimiq
/// transaction (the G12 "free spam filter", RISKS.md #4): decode it, derive the sender from
/// the embedded public key, rebuild `serializeContent`, and check the Ed25519 signature with
/// `verify_strict` (rejects non-canonical / small-order points).
///
/// This is the relay's pre-flood guard. It is deliberately **content-blind** — it proves the
/// blob is a genuine signed transfer (so the mesh can't be flooded with junk), but never
/// inspects *who* it pays or *whether it can pay* (core value #3: trustless relay; balance
/// is the gateway's / chain's job). Pure, panic-free, no key material crosses.
pub fn verify_basic_wire(wire: &[u8]) -> bool {
    let Some(decoded) = decode_basic_wire(wire) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&decoded.public_key) else {
        return false;
    };
    let sig = Signature::from_bytes(&decoded.signature);
    vk.verify_strict(&decoded.transfer.serialize_content(), &sig)
        .is_ok()
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

    /// Sign a real transfer with a fixed test key and return `(wire, sender)`.
    fn signed_wire() -> (Vec<u8>, Address) {
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pubkey = sk.verifying_key().to_bytes();
        let sender = Address::from_public_key(&pubkey);
        let t = Transfer {
            sender,
            recipient: addr("567866611c1a2c85a1a676bb8c845d0655fea1a6"),
            value: 250_000,
            fee: 0,
            validity_start_height: 4_428_402,
            network_id: 5,
        };
        let sig = sk.sign(&t.serialize_content()).to_bytes();
        (t.serialize_basic(&pubkey, &sig), sender)
    }

    #[test]
    fn decode_round_trips_a_signed_wire() {
        let (wire, sender) = signed_wire();
        let d = decode_basic_wire(&wire).expect("decodes");
        assert_eq!(d.transfer.sender, sender); // derived from the embedded pubkey
        assert_eq!(d.transfer.value, 250_000);
        assert_eq!(d.transfer.validity_start_height, 4_428_402);
        assert_eq!(d.transfer.network_id, 5);
        // Re-encoding the decoded fields reproduces the exact wire.
        assert_eq!(
            d.transfer.serialize_basic(&d.public_key, &d.signature),
            wire
        );
    }

    #[test]
    fn verify_accepts_a_real_signature() {
        let (wire, _) = signed_wire();
        assert!(verify_basic_wire(&wire));
    }

    #[test]
    fn verify_rejects_tampering_and_junk() {
        let (mut wire, _) = signed_wire();
        // Flip a byte in the value field → signature no longer matches.
        wire[55] ^= 0x01;
        assert!(!verify_basic_wire(&wire));

        // A flipped signature byte fails too.
        let (mut wire2, _) = signed_wire();
        let last = wire2.len() - 1;
        wire2[last] ^= 0x01;
        assert!(!verify_basic_wire(&wire2));

        // Wrong length, wrong format byte, and arbitrary junk all fail (decode → None).
        assert!(!verify_basic_wire(b"not-a-real-signed-tx"));
        assert!(decode_basic_wire(b"").is_none());
        let (mut bad_format, _) = signed_wire();
        bad_format[0] = 0x01; // not FORMAT_BASIC
        assert!(decode_basic_wire(&bad_format).is_none());
    }
}
