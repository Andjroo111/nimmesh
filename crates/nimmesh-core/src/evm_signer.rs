//! # evm_signer — the real secp256k1 `EvmSigner` for the USDC-on-Polygon leg (P4c, behind `polygon-leg`)
//!
//! A real [`crate::evm_rlp::EvmSigner`] over secp256k1, the EVM mirror of `btc::BtcEnclaveKey`. It
//! signs a 32-byte EIP-155 signing hash with **RFC-6979 deterministic ECDSA** (no RNG → reproducible,
//! never flaky) and **EIP-2 low-s normalization**, returning `(r, s, recovery_id)`. The 32-byte
//! secret lives behind the [`LocalEvmKey`] seam and never leaves it — only the signature + the public
//! address are exposed; the secret-bearing constructor is **not** on the UniFFI surface.
//!
//! **Testnet only / money-path gated.** This produces real signatures, but the loop only ever signs
//! against the PUBLIC EIP-155 test key in tests, our own txs use Polygon **Amoy** (chainId 80002),
//! and nothing here broadcasts. Signing with a real funded key, on mainnet, or broadcasting is
//! owner-gated (needs:owner). Validated end to end against the published EIP-155 spec vector.

use crate::evm::evm_address;
use crate::evm_rlp::EvmSigner;
use k256::ecdsa::SigningKey;

/// A local secp256k1 signing key for EVM transactions. The secret is held behind this seam (the EVM
/// mirror of `btc::InMemoryBtcEnclaveKey`): only signatures + the derived address leave it. For
/// tests/dev and on-device use; the secret-bearing constructor is never FFI-exported.
pub struct LocalEvmKey {
    signing: SigningKey,
}

/// A bad secret scalar (zero or ≥ the secp256k1 group order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEvmSecret;

impl LocalEvmKey {
    /// Build from a 32-byte big-endian secret scalar. Errors if the scalar is not a valid secp256k1
    /// private key (zero or ≥ n).
    pub fn from_secret(secret: &[u8; 32]) -> Result<Self, InvalidEvmSecret> {
        let signing = SigningKey::from_bytes(secret.into()).map_err(|_| InvalidEvmSecret)?;
        Ok(LocalEvmKey { signing })
    }

    /// The 64-byte uncompressed public key `X || Y` (no `0x04` prefix) — the input to EVM address
    /// derivation.
    pub fn uncompressed_pubkey(&self) -> [u8; 64] {
        let point = self.signing.verifying_key().to_encoded_point(false);
        let bytes = point.as_bytes(); // 0x04 || X || Y (65 bytes)
        let mut out = [0u8; 64];
        out.copy_from_slice(&bytes[1..]);
        out
    }

    /// The 20-byte EVM account address for this key (`keccak256(X || Y)[12..]`).
    pub fn address(&self) -> [u8; 20] {
        evm_address(&self.uncompressed_pubkey())
    }
}

impl EvmSigner for LocalEvmKey {
    fn sign_hash(&self, hash: [u8; 32]) -> ([u8; 32], [u8; 32], u8) {
        // Deterministic (RFC-6979) signing over the 32-byte prehash; k256 normalizes `s` to the low
        // half (EIP-2) and yields the matching recovery id.
        let (sig, recid) = self
            .signing
            .sign_prehash_recoverable(&hash)
            .expect("a 32-byte prehash is always signable");
        let bytes = sig.to_bytes(); // 64 bytes: r (32) || s (32)
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&bytes[..32]);
        s.copy_from_slice(&bytes[32..]);
        (r, s, recid.to_byte())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm_rlp::LegacyTx;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn h32(s: &str) -> [u8; 32] {
        let mut a = [0u8; 32];
        a.copy_from_slice(&hex(s));
        a
    }

    // The PUBLIC EIP-155 spec test private key. NOT a real/funded key — only used to reproduce the
    // published vector.
    const TEST_SECRET: [u8; 32] = [0x46; 32];

    #[test]
    fn address_for_the_eip155_test_key_matches_the_known_value() {
        let key = LocalEvmKey::from_secret(&TEST_SECRET).unwrap();
        // The well-known address for private key 0x4646…46.
        assert_eq!(
            key.address().to_vec(),
            hex("9d8a62f656a8d1615c1294fd71e9cfb3e4855a4f")
        );
    }

    #[test]
    fn signing_the_canonical_tx_reproduces_the_published_rsv_vector() {
        // Build the canonical EIP-155 example tx (chainId 1) and sign its signing hash with the test
        // key; RFC-6979 makes this deterministic, so it must equal the published r/s/recovery.
        let mut to = [0u8; 20];
        to.copy_from_slice(&hex("3535353535353535353535353535353535353535"));
        let tx = LegacyTx {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to,
            value: 1_000_000_000_000_000_000,
            data: &[],
            chain_id: 1,
        };
        let key = LocalEvmKey::from_secret(&TEST_SECRET).unwrap();
        let (r, s, recovery) = key.sign_hash(tx.signing_hash());
        assert_eq!(
            r,
            h32("28ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276")
        );
        assert_eq!(
            s,
            h32("67cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83")
        );
        assert_eq!(recovery, 0);
    }

    #[test]
    fn sign_with_reproduces_the_published_raw_signed_tx() {
        // End to end: the real signer driving the P4b assembly yields the exact published raw tx.
        let mut to = [0u8; 20];
        to.copy_from_slice(&hex("3535353535353535353535353535353535353535"));
        let tx = LegacyTx {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to,
            value: 1_000_000_000_000_000_000,
            data: &[],
            chain_id: 1,
        };
        let key = LocalEvmKey::from_secret(&TEST_SECRET).unwrap();
        assert_eq!(
            tx.sign_with(&key),
            hex("f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83")
        );
    }

    #[test]
    fn signing_is_deterministic_rfc6979() {
        // Same key + same hash → identical signature every time (no RNG; never flaky).
        let key = LocalEvmKey::from_secret(&TEST_SECRET).unwrap();
        let hash = h32("daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53");
        assert_eq!(key.sign_hash(hash), key.sign_hash(hash));
    }

    #[test]
    fn a_zero_secret_is_rejected() {
        assert!(matches!(
            LocalEvmKey::from_secret(&[0u8; 32]),
            Err(InvalidEvmSecret)
        ));
    }
}
