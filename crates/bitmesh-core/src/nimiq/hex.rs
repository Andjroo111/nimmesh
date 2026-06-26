//! # nimiq::hex — pure hex + big-endian integer byte helpers (no chain dependency)
//!
//! Ported from the fleet's proven `sendhome/src/htlc/hex.ts` (originally the Hashmark
//! HTLC CLI), kept byte-identical so the Albatross serializer here drops onto the same
//! deterministic wire logic the rest of the fleet already proved against `@nimiq/core`.
//! Everything is allocation-light, infallible on the encode path, and `no_std`-shaped
//! (only `Vec`/`String`). Used by [`super::tx`] (content/wire serialization), the
//! [`super::address`] base32 codec, and the test harness's fixture decode.

/// Lowercase-hex-encode a byte slice (e.g. `[0xab, 0x01] -> "ab01"`).
///
/// This is the canonical rendering for a signed-tx `rawHex` and a `txHash` so the value
/// crossing the FFI boundary matches `@nimiq/core`'s `tx.serialize()`/`tx.hash()` exactly.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `{:02x}` is the same zero-padded lowercase nibble pair as the TS `padStart(2,"0")`.
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a hex string (optionally `0x`-prefixed) into bytes. Errors on odd length or a
/// non-hex nibble — the mesh treats all input as hostile, so a malformed blob is rejected
/// rather than silently truncated.
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, HexError> {
    let clean = hex.strip_prefix("0x").unwrap_or(hex);
    if clean.len() % 2 != 0 {
        return Err(HexError::OddLength(clean.len()));
    }
    let bytes = clean.as_bytes();
    let mut out = Vec::with_capacity(clean.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// Parse a single ASCII hex nibble (`0-9a-fA-F`) into its 0–15 value.
fn nibble(c: u8) -> Result<u8, HexError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(HexError::BadNibble(c as char)),
    }
}

/// A failure decoding hex bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HexError {
    /// The (un-prefixed) string had an odd number of characters.
    OddLength(usize),
    /// A character outside `[0-9a-fA-F]` was encountered.
    BadNibble(char),
}

impl std::fmt::Display for HexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HexError::OddLength(n) => write!(f, "hex length odd: {n}"),
            HexError::BadNibble(c) => write!(f, "invalid hex nibble: {c:?}"),
        }
    }
}

impl std::error::Error for HexError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_all_bytes() {
        let all: Vec<u8> = (0..=255u8).collect();
        let hex = bytes_to_hex(&all);
        assert_eq!(hex.len(), 512);
        assert_eq!(hex_to_bytes(&hex).unwrap(), all);
    }

    #[test]
    fn encodes_zero_padded_lowercase() {
        assert_eq!(bytes_to_hex(&[0x0a, 0xb0, 0x00, 0xff]), "0ab000ff");
        assert_eq!(bytes_to_hex(&[]), "");
    }

    #[test]
    fn accepts_0x_prefix_and_uppercase() {
        assert_eq!(hex_to_bytes("0xAB01").unwrap(), vec![0xab, 0x01]);
        assert_eq!(hex_to_bytes("ab01").unwrap(), vec![0xab, 0x01]);
    }

    #[test]
    fn rejects_odd_length_and_bad_nibble() {
        assert_eq!(hex_to_bytes("abc"), Err(HexError::OddLength(3)));
        assert_eq!(hex_to_bytes("zz"), Err(HexError::BadNibble('z')));
    }
}
