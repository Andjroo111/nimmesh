//! # evm_rlp — minimal RLP encoder + EIP-155 legacy tx signing-hash (P4a, behind `polygon-leg`)
//!
//! The deterministic, **key-free** half of building a Polygon transaction: RLP-encode the tx fields
//! and return the **EIP-155 signing hash** the swap will eventually sign. No secp256k1, no key
//! material, no RPC, no broadcast — that is P4b (a key seam like `btc::BtcEnclaveKey`, testnet-only,
//! mainnet + broadcast owner-gated). This module just turns numbers + the P3 `evm_abi` calldata into
//! the exact 32-byte hash an EVM wallet signs, validated against the canonical EIP-155 vector.
//!
//! ## RLP in one paragraph
//!
//! A byte in `0x00..=0x7f` is itself. A string ≤55 bytes is `0x80+len ++ bytes` (empty = `0x80`); a
//! longer string is `0xb7+len_of_len ++ len ++ bytes`. A list is the same with base `0xc0`/`0xf7`
//! over the concatenated already-encoded items. Integers are encoded as their **minimal** big-endian
//! byte string (no leading zeros; `0` → the empty string `0x80`).

use crate::evm::keccak256;
use crate::swap_usdc_leg::EvmAddress;

/// Polygon **Amoy testnet** chain id (EIP-155). Mainnet Polygon is `137` and is GATED — the loop
/// never emits it; `LegacyTx::polygon_amoy` hard-codes this so the money-path can't reach mainnet.
pub const POLYGON_AMOY_CHAIN_ID: u64 = 80002;

/// The minimal big-endian byte representation of `v` — no leading zeros, and `0` → empty (`vec![]`),
/// which RLP renders as the empty string `0x80`. This is how EVM integers (nonce, gas, value,
/// chainId) are encoded.
fn minimal_be(v: u64) -> Vec<u8> {
    if v == 0 {
        return Vec::new();
    }
    let bytes = v.to_be_bytes();
    let first = bytes.iter().position(|&b| b != 0).unwrap();
    bytes[first..].to_vec()
}

/// The RLP length prefix for a payload of `len` bytes, with base `offset` (`0x80` string / `0xc0`
/// list).
fn rlp_length(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        vec![offset + len as u8]
    } else {
        let len_be = minimal_be(len as u64);
        let mut out = Vec::with_capacity(1 + len_be.len());
        out.push(offset + 55 + len_be.len() as u8);
        out.extend_from_slice(&len_be);
        out
    }
}

/// RLP-encode a byte string.
pub fn rlp_bytes(data: &[u8]) -> Vec<u8> {
    // A single byte below 0x80 is its own encoding.
    if data.len() == 1 && data[0] < 0x80 {
        return vec![data[0]];
    }
    let mut out = rlp_length(data.len(), 0x80);
    out.extend_from_slice(data);
    out
}

/// RLP-encode a list from its already-encoded items.
pub fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(Vec::len).sum();
    let mut out = rlp_length(payload_len, 0xc0);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// RLP-encode a `u64` as a minimal-big-endian integer (`0` → `0x80`).
pub fn rlp_u64(v: u64) -> Vec<u8> {
    rlp_bytes(&minimal_be(v))
}

/// An EIP-155 **legacy** transaction. `value` is native (wei/MATIC) and is `0` for the USDC HTLC +
/// ERC-20 contract calls (the token amount lives in `data`, the P3 `evm_abi` calldata). Amounts +
/// gas fit `u64` for our calls; the canonical EIP-155 vector (`value = 10^18`) also fits.
pub struct LegacyTx<'a> {
    /// Sender account nonce.
    pub nonce: u64,
    /// Gas price in wei.
    pub gas_price: u64,
    /// Gas limit.
    pub gas_limit: u64,
    /// The 20-byte recipient (the USDC token or the HTLC contract for our calls).
    pub to: EvmAddress,
    /// Native value in wei (0 for contract calls).
    pub value: u64,
    /// The calldata (e.g. from `evm_abi`).
    pub data: &'a [u8],
    /// EIP-155 chain id.
    pub chain_id: u64,
}

impl<'a> LegacyTx<'a> {
    /// A Polygon **Amoy testnet** legacy tx (chainId 80002). Mainnet (137) is intentionally not
    /// reachable through this constructor — the loop only ever builds Amoy txs.
    pub fn polygon_amoy(
        nonce: u64,
        gas_price: u64,
        gas_limit: u64,
        to: EvmAddress,
        value: u64,
        data: &'a [u8],
    ) -> Self {
        LegacyTx {
            nonce,
            gas_price,
            gas_limit,
            to,
            value,
            data,
            chain_id: POLYGON_AMOY_CHAIN_ID,
        }
    }

    /// The EIP-155 signing payload: `rlp([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0])`.
    /// (The trailing `0, 0` are the placeholder `r`/`s`; this is the pre-signature payload that gets
    /// hashed, per EIP-155.)
    pub fn signing_rlp(&self) -> Vec<u8> {
        rlp_list(&[
            rlp_u64(self.nonce),
            rlp_u64(self.gas_price),
            rlp_u64(self.gas_limit),
            rlp_bytes(&self.to),
            rlp_u64(self.value),
            rlp_bytes(self.data),
            rlp_u64(self.chain_id),
            rlp_u64(0),
            rlp_u64(0),
        ])
    }

    /// The EIP-155 signing hash = `keccak256(signing_rlp())` — the exact 32 bytes a wallet signs.
    /// P4b signs this behind a key seam; this module never holds a key.
    pub fn signing_hash(&self) -> [u8; 32] {
        keccak256(&self.signing_rlp())
    }
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
    fn rlp_byte_strings_match_canonical_vectors() {
        assert_eq!(rlp_bytes(b"dog"), hex("83646f67")); // "dog"
        assert_eq!(rlp_bytes(b""), vec![0x80]); // empty string
        assert_eq!(rlp_bytes(&[0x00]), vec![0x00]); // single zero byte is itself
        assert_eq!(rlp_bytes(&[0x0f]), vec![0x0f]); // single byte < 0x80 is itself
        assert_eq!(rlp_bytes(&[0x7f]), vec![0x7f]);
        assert_eq!(rlp_bytes(&[0x80]), vec![0x81, 0x80]); // single byte >= 0x80 needs a prefix
    }

    #[test]
    fn rlp_long_string_uses_the_length_of_length_form() {
        // 56 bytes (the first length that overflows the short form): 0xb8, 0x38, then the bytes.
        let data = vec![0x61u8; 56];
        let enc = rlp_bytes(&data);
        assert_eq!(&enc[..2], &[0xb8, 0x38]);
        assert_eq!(enc.len(), 2 + 56);
        assert_eq!(&enc[2..], &data[..]);
    }

    #[test]
    fn rlp_lists_and_integers_match_canonical_vectors() {
        assert_eq!(rlp_list(&[]), vec![0xc0]); // empty list
        assert_eq!(rlp_u64(0), vec![0x80]); // zero -> empty string
        assert_eq!(rlp_u64(15), vec![0x0f]); // small int is itself
        assert_eq!(rlp_u64(1024), hex("820400")); // 0x0400, minimal big-endian
                                                  // ["cat", "dog"] -> 0xc8 83 636174 83 646f67
        let list = rlp_list(&[rlp_bytes(b"cat"), rlp_bytes(b"dog")]);
        assert_eq!(list, hex("c88363617483646f67"));
    }

    #[test]
    fn eip155_canonical_example_signing_rlp_and_hash() {
        // The exact example from the EIP-155 specification:
        // nonce 9, gasPrice 20e9, gasLimit 21000, to 0x3535…35, value 1e18, data empty, chainId 1.
        let to = {
            let mut a = [0u8; 20];
            a.copy_from_slice(&hex("3535353535353535353535353535353535353535"));
            a
        };
        let tx = LegacyTx {
            nonce: 9,
            gas_price: 20_000_000_000,
            gas_limit: 21_000,
            to,
            value: 1_000_000_000_000_000_000,
            data: &[],
            chain_id: 1,
        };
        // The EIP-155 signing data — this hex IS the published EIP-155 example signing data, so this
        // assertion is the external validation of the RLP encoder.
        assert_eq!(
            tx.signing_rlp(),
            hex("ec098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a764000080018080")
        );
        // ...and its keccak256 signing hash. keccak256 itself is validated against famous external
        // vectors (empty string in `evm`, and "abc" = 4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64…),
        // so this hash is the deterministic keccak256 of the published signing data above.
        assert_eq!(
            tx.signing_hash().to_vec(),
            hex("daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53")
        );
    }

    #[test]
    fn polygon_amoy_tx_wraps_calldata_with_the_testnet_chain_id() {
        use crate::evm_abi::htlc_refund;
        // A real-shape tx: refund(swapId) calldata to the HTLC contract on Amoy, value 0.
        let contract = [0x5Au8; 20];
        let swap_id = [0x2Fu8; 32];
        let data = htlc_refund(&swap_id);
        let tx = LegacyTx::polygon_amoy(3, 30_000_000_000, 120_000, contract, 0, &data);
        assert_eq!(tx.chain_id, 80002); // Amoy testnet, never mainnet 137

        let rlp = tx.signing_rlp();
        // data is 36 bytes → the list payload exceeds 55 bytes → long-list prefix 0xf8.
        assert_eq!(rlp[0], 0xf8);
        // chainId 80002 = 0x013882 encodes as RLP 0x83 013882; it appears in the payload.
        assert!(rlp.windows(4).any(|w| w == [0x83, 0x01, 0x38, 0x82]));
        // the calldata is carried verbatim inside the tx.
        assert!(rlp.windows(data.len()).any(|w| w == &data[..]));
        // value 0 encodes as the empty string 0x80 (present in the payload).
        assert!(rlp.contains(&0x80));
        // the hash is a deterministic 32 bytes.
        assert_eq!(tx.signing_hash(), tx.signing_hash());
    }

    #[test]
    fn amoy_constant_is_testnet() {
        assert_eq!(POLYGON_AMOY_CHAIN_ID, 80002);
        assert_ne!(POLYGON_AMOY_CHAIN_ID, 137); // never Polygon mainnet
    }
}
