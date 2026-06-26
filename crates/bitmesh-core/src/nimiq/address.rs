//! # nimiq::address — the 20-byte Nimiq address + its user-friendly NQ-IBAN codec
//!
//! A Nimiq address is 20 raw bytes (`Blake2b-256(public_key)[..20]`). Humans see it as a
//! 36-character **user-friendly** string `NQxx XXXX XXXX ...` — an ISO-13616 IBAN-style
//! envelope: `"NQ"` + two mod-97 check digits + the 20 bytes rendered in Nimiq's custom
//! base32 alphabet, grouped in fours. This module is the pure, offline-only codec: parse
//! (with checksum verification) and format, byte-for-byte matching
//! `@nimiq/core`'s `Address.fromUserFriendlyAddress` / `toUserFriendlyAddress`
//! (the validator the fleet's `sendhome`/`nimiq.sale` already lean on). No network, no
//! WASM — a typo'd/transposed address is caught here by the checksum, never mis-routed.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

use super::hex::bytes_to_hex;

/// Length in bytes of a raw Nimiq address.
pub const ADDRESS_LEN: usize = 20;

/// Nimiq's base32 alphabet (RFC-4648 base32 minus the ambiguous `I`, `O`, `W`, `Z`).
/// 32 symbols, so 20 bytes (160 bits) encode to exactly 32 characters with no padding.
const BASE32_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKLMNPQRSTUVXY";

/// A Nimiq account address: 20 raw bytes. Copyable value type; never holds key material.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address(pub [u8; ADDRESS_LEN]);

impl Address {
    /// Wrap 20 raw bytes as an address.
    pub const fn from_bytes(bytes: [u8; ADDRESS_LEN]) -> Self {
        Address(bytes)
    }

    /// The 20 raw bytes (the on-wire form used in `serializeContent`).
    pub const fn as_bytes(&self) -> &[u8; ADDRESS_LEN] {
        &self.0
    }

    /// Derive the address from a 32-byte Ed25519 public key: `Blake2b-256(pubkey)[..20]`.
    ///
    /// This is the only place key-*public* material enters the codec; the secret never does
    /// (core value #1). Matches `KeyPair::toAddress()` in `@nimiq/core`.
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let mut hasher = Blake2b::<U32>::new();
        hasher.update(public_key);
        let digest = hasher.finalize();
        let mut out = [0u8; ADDRESS_LEN];
        out.copy_from_slice(&digest[..ADDRESS_LEN]);
        Address(out)
    }

    /// Parse a user-friendly `NQ…` address, verifying its IBAN checksum.
    ///
    /// Whitespace grouping is insignificant (different surfaces group differently) so it is
    /// stripped first; the string must start with `NQ`, carry valid base32, and pass the
    /// mod-97 check — otherwise the address is rejected rather than silently mis-decoded.
    pub fn from_user_friendly(input: &str) -> Result<Self, AddressError> {
        let compact: String = input
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if compact.len() != 36 {
            return Err(AddressError::WrongLength(compact.len()));
        }
        if &compact[..2] != "NQ" {
            return Err(AddressError::MissingPrefix);
        }
        let check = &compact[2..4];
        let base32 = &compact[4..];
        // IBAN verify: rearrange to `base32 + country + check`, mod-97 must equal 1.
        if iban_mod97(&format!("{base32}NQ{check}")) != 1 {
            return Err(AddressError::BadChecksum);
        }
        let raw = base32_decode(base32)?;
        Ok(Address(raw))
    }

    /// Render the canonical user-friendly form `NQxx XXXX … XXXX` (groups of four).
    ///
    /// Computes the two mod-97 check digits over `base32 + "NQ00"` exactly as
    /// `@nimiq/core`, so the output round-trips through [`Address::from_user_friendly`].
    pub fn to_user_friendly(&self) -> String {
        let base32 = base32_encode(&self.0);
        let check = 98 - iban_mod97(&format!("{base32}NQ00"));
        let body = format!("NQ{check:02}{base32}");
        // Group into 4-char blocks separated by single spaces (the wallet's rendering).
        let mut grouped = String::with_capacity(body.len() + body.len() / 4);
        for (i, c) in body.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                grouped.push(' ');
            }
            grouped.push(c);
        }
        grouped
    }

    /// The raw 20 bytes as lowercase hex (debugging / fixtures).
    pub fn to_hex(&self) -> String {
        bytes_to_hex(&self.0)
    }
}

impl std::fmt::Debug for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Address({})", self.to_user_friendly())
    }
}

/// A failure parsing or decoding a Nimiq address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    /// The compacted string was not 36 characters (`NQ` + 2 check + 32 base32).
    WrongLength(usize),
    /// The string did not start with the `NQ` country code.
    MissingPrefix,
    /// The IBAN mod-97 checksum did not validate (typo / transposition).
    BadChecksum,
    /// A character outside the Nimiq base32 alphabet was encountered.
    BadBase32Char(char),
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::WrongLength(n) => write!(f, "address must be 36 chars, got {n}"),
            AddressError::MissingPrefix => write!(f, "address must start with NQ"),
            AddressError::BadChecksum => write!(f, "address checksum failed"),
            AddressError::BadBase32Char(c) => write!(f, "invalid base32 char: {c:?}"),
        }
    }
}

impl std::error::Error for AddressError {}

/// Streaming ISO-7064 mod-97-10 over a base36-mapped string (`A`=10 … `Z`=35, digits as-is).
///
/// Processing digit-by-digit (`tmp = (tmp*10 + d) % 97`) is arithmetically identical to the
/// 6-char-chunk big-number reduction `@nimiq/core` does, and avoids any big-int dependency.
fn iban_mod97(s: &str) -> u32 {
    let mut tmp = 0u32;
    for c in s.chars() {
        if let Some(d) = c.to_digit(10) {
            tmp = (tmp * 10 + d) % 97;
        } else {
            // Letters expand to a two-digit number 10..=35.
            let v = (c.to_ascii_uppercase() as u32) - ('A' as u32) + 10;
            tmp = (tmp * 10 + v / 10) % 97;
            tmp = (tmp * 10 + v % 10) % 97;
        }
    }
    tmp
}

/// Encode 20 bytes (160 bits) into exactly 32 Nimiq-base32 characters (no padding).
fn base32_encode(bytes: &[u8; ADDRESS_LEN]) -> String {
    let mut out = String::with_capacity(32);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    // 160 bits / 5 = 32 chars exactly; no leftover bits to flush.
    out
}

/// Decode exactly 32 Nimiq-base32 characters back into 20 bytes.
fn base32_decode(s: &str) -> Result<[u8; ADDRESS_LEN], AddressError> {
    let mut out = [0u8; ADDRESS_LEN];
    let mut idx = 0usize;
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for c in s.chars() {
        let val = BASE32_ALPHABET
            .iter()
            .position(|&a| a as char == c)
            .ok_or(AddressError::BadBase32Char(c))? as u32;
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[idx] = ((buffer >> bits) & 0xff) as u8;
            idx += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::hex::hex_to_bytes;

    fn addr(hex: &str) -> Address {
        let mut b = [0u8; 20];
        b.copy_from_slice(&hex_to_bytes(hex).unwrap());
        Address(b)
    }

    #[test]
    fn formats_matches_nimiq_core_reference() {
        // From the @nimiq/core fixture (seq-keys-100k-vsh1234).
        let a = addr("4d4dbe917544b07922348a66b9c4b5a5a5f34a9f");
        assert_eq!(
            a.to_user_friendly(),
            "NQ65 9M6T V4BM 8JQ7 J8HL H9KB KH5M LNJY 6JLY"
        );
    }

    #[test]
    fn parse_round_trips_canonical_form() {
        let user = "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6";
        let a = Address::from_user_friendly(user).unwrap();
        assert_eq!(a.to_hex(), "567866611c1a2c85a1a676bb8c845d0655fea1a6");
        assert_eq!(a.to_user_friendly(), user);
    }

    #[test]
    fn parse_tolerates_no_spaces_and_lowercase() {
        // (lowercase, contiguous) — still validates and decodes the same bytes as the
        // canonical "NQ95 ARU6 CQ8U 38N8 B8D6 ESVQ R12V 0RAY V8D6".
        let a = Address::from_user_friendly("nq95aru6cq8u38n8b8d6esvqr12v0rayv8d6");
        assert!(a.is_ok(), "parse failed: {a:?}");
        assert_eq!(
            a.unwrap().to_hex(),
            "567866611c1a2c85a1a676bb8c845d0655fea1a6"
        );
    }

    #[test]
    fn derives_address_from_public_key() {
        let pk = hex_to_bytes("79b5562e8fe654f94078b112e8a98ba7901f853ae695bed7e0e3910bad049664")
            .unwrap();
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(&pk);
        let a = Address::from_public_key(&pubkey);
        assert_eq!(a.to_hex(), "4d4dbe917544b07922348a66b9c4b5a5a5f34a9f");
    }

    #[test]
    fn rejects_bad_checksum_and_prefix() {
        // Flip a check digit -> checksum fails.
        assert_eq!(
            Address::from_user_friendly("NQ66 9M6T V4BM 8JQ7 J8HL H9KB KH5M LNJY 6JLY"),
            Err(AddressError::BadChecksum)
        );
        assert_eq!(
            Address::from_user_friendly("XX65 9M6T V4BM 8JQ7 J8HL H9KB KH5M LNJY 6JLY"),
            Err(AddressError::MissingPrefix)
        );
    }
}
