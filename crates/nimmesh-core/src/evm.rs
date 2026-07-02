//! # evm — EVM (Polygon) primitives for the USDC counterparty leg (P1, behind `polygon-leg`)
//!
//! Keccak-256 plus the two derivations every EVM interaction needs: an account ADDRESS from a
//! secp256k1 public key, and an ABI 4-byte function SELECTOR from a signature. No keys, no network —
//! the USDC HTLC leg (later goals) builds on these.
//!
//! NOTE: the EVM hashes with **Keccak-256** (`sha3::Keccak256`), which is NOT NIST SHA3-256 (they
//! differ only in the padding byte). The cross-chain swap **hashlock stays SHA-256** (shared with the
//! NIM and BTC legs, so one secret unlocks all three); keccak here is ONLY for EVM addressing + ABI.

use sha3::{Digest, Keccak256};

/// EVM Keccak-256 of `data`.
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// The 20-byte EVM account address for a secp256k1 public key. `pubkey` is the **uncompressed** key
/// WITHOUT the `0x04` prefix — the 64-byte `X || Y`. The address is `keccak256(X || Y)[12..32]`.
pub fn evm_address(pubkey: &[u8; 64]) -> [u8; 20] {
    let h = keccak256(pubkey);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&h[12..]);
    addr
}

/// The 4-byte ABI function selector for a canonical signature, e.g. `"transfer(address,uint256)"`.
pub fn function_selector(signature: &str) -> [u8; 4] {
    let h = keccak256(signature.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn keccak256_matches_the_empty_string_vector() {
        // Canonical Keccak-256("") — distinct from SHA3-256("") (proves we use the EVM variant).
        assert_eq!(
            keccak256(b"").to_vec(),
            hex("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        );
    }

    #[test]
    fn address_for_privkey_one_matches_the_known_vector() {
        // secp256k1 private key `1` → public key is the generator G; its EVM address is the well-known
        // 0x7e5f4552091a69125d5dfcb7b8c2659029395bdf.
        let mut pubkey = [0u8; 64];
        pubkey[..32].copy_from_slice(&hex(
            "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        ));
        pubkey[32..].copy_from_slice(&hex(
            "483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8",
        ));
        assert_eq!(
            evm_address(&pubkey).to_vec(),
            hex("7e5f4552091a69125d5dfcb7b8c2659029395bdf")
        );
    }

    #[test]
    fn function_selectors_match_known_erc20_methods() {
        assert_eq!(
            function_selector("transfer(address,uint256)"),
            [0xa9, 0x05, 0x9c, 0xbb]
        );
        assert_eq!(
            function_selector("balanceOf(address)"),
            [0x70, 0xa0, 0x82, 0x31]
        );
        assert_eq!(
            function_selector("transferFrom(address,address,uint256)"),
            [0x23, 0xb8, 0x72, 0xdd]
        );
    }
}
