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

/// RLP-encode a big-endian integer given as bytes (e.g. a 32-byte signature `r`/`s`): strip leading
/// zero bytes to the minimal representation, then encode as an RLP string (all-zero → empty → `0x80`).
pub fn rlp_int(be: &[u8]) -> Vec<u8> {
    let first = be.iter().position(|&b| b != 0).unwrap_or(be.len());
    rlp_bytes(&be[first..])
}

/// The EIP-155 `v` value for a signature: `recovery_id + chain_id*2 + 35` (replaces the bare `0/1`
/// recovery byte so the chain id is bound into the signature, preventing cross-chain replay).
pub fn eip155_v(recovery_id: u8, chain_id: u64) -> u64 {
    recovery_id as u64 + chain_id * 2 + 35
}

/// Signs a 32-byte EIP-155 signing hash, returning the ECDSA signature as big-endian 32-byte `r`,
/// 32-byte `s`, and the `0`/`1` recovery id. This is the SEAM the real secp256k1 signer (P4c, gated
/// behind a key) plugs into — analogous to `btc::BtcEnclaveKey`. This module never holds a key; the
/// signed-tx assembly takes the signature as input, so it stays pure + key-free + deterministic.
pub trait EvmSigner {
    /// Sign `hash` (the [`LegacyTx::signing_hash`]), returning `(r, s, recovery_id)`.
    fn sign_hash(&self, hash: [u8; 32]) -> ([u8; 32], [u8; 32], u8);
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

    /// Assemble the RAW SIGNED transaction RLP from a signature: `rlp([nonce, gasPrice, gasLimit, to,
    /// value, data, v, r, s])` where `v = eip155_v(recovery_id, chainId)` and `r`/`s` are RLP integers
    /// (minimal big-endian). KEY-FREE byte assembly — `(r, s, recovery_id)` come from an
    /// [`EvmSigner`]; the result is the bytes a node would broadcast (broadcast itself is P4c+, gated).
    pub fn signed_tx_rlp(&self, r: &[u8; 32], s: &[u8; 32], recovery_id: u8) -> Vec<u8> {
        rlp_list(&[
            rlp_u64(self.nonce),
            rlp_u64(self.gas_price),
            rlp_u64(self.gas_limit),
            rlp_bytes(&self.to),
            rlp_u64(self.value),
            rlp_bytes(self.data),
            rlp_u64(eip155_v(recovery_id, self.chain_id)),
            rlp_int(r),
            rlp_int(s),
        ])
    }

    /// Sign + assemble in one step through the [`EvmSigner`] seam: hash → `signer.sign_hash` →
    /// `signed_tx_rlp`. The real signer (P4c, gated) is the only piece that touches a key.
    pub fn sign_with<S: EvmSigner>(&self, signer: &S) -> Vec<u8> {
        let (r, s, recovery_id) = signer.sign_hash(self.signing_hash());
        self.signed_tx_rlp(&r, &s, recovery_id)
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

    #[test]
    fn eip155_v_binds_chain_id_and_recovery() {
        // The EIP-155 spec example: recovery 0, chainId 1 → v = 37.
        assert_eq!(eip155_v(0, 1), 37);
        assert_eq!(eip155_v(1, 1), 38);
        // Polygon Amoy (80002): v = recovery + 80002*2 + 35.
        assert_eq!(eip155_v(0, POLYGON_AMOY_CHAIN_ID), 160_039);
        assert_eq!(eip155_v(1, POLYGON_AMOY_CHAIN_ID), 160_040);
    }

    #[test]
    fn rlp_int_strips_leading_zeros() {
        assert_eq!(rlp_int(&[0x00, 0x00, 0x05]), vec![0x05]); // 0x000005 → 0x05
        assert_eq!(rlp_int(&[0x00; 32]), vec![0x80]); // all-zero → empty string
        assert_eq!(rlp_int(&[0x01, 0x00]), hex("820100")); // 0x0100, no strip
    }

    // A test-only signer that returns a fixed `(r, s, recovery)` — it exercises the `EvmSigner` seam
    // WITHOUT any secp256k1/key (the real signer is P4c, gated). Here it returns the published
    // EIP-155 test vector's signature.
    struct VectorSigner {
        r: [u8; 32],
        s: [u8; 32],
        recovery: u8,
    }
    impl EvmSigner for VectorSigner {
        fn sign_hash(&self, _hash: [u8; 32]) -> ([u8; 32], [u8; 32], u8) {
            (self.r, self.s, self.recovery)
        }
    }

    fn h32(s: &str) -> [u8; 32] {
        let mut a = [0u8; 32];
        a.copy_from_slice(&hex(s));
        a
    }

    #[test]
    fn signed_tx_rlp_matches_the_published_eip155_test_key_vector() {
        // The EIP-155 spec's SIGNED example: private key 0x4646…4646 signing the canonical tx yields
        // v=37, r=0x28ef61…6276, s=0x67cbe9…6d83, and this exact raw signed transaction. We feed the
        // published (r, s, recovery=0) into the assembly and assert the raw tx byte-for-byte — the
        // external validation of the signed-tx RLP layout + the EIP-155 `v`.
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
        let r = h32("28ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276");
        let s = h32("67cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83");
        let raw = tx.signed_tx_rlp(&r, &s, 0);
        assert_eq!(
            raw,
            hex("f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83")
        );
        // v = 37 = 0x25, sitting right after the empty-data 0x80.
        assert!(raw.windows(2).any(|w| w == [0x80, 0x25]));

        // ...and the same result is reached through the EvmSigner seam (the path the real P4c signer
        // will take), proving the seam drives the assembly.
        let signer = VectorSigner { r, s, recovery: 0 };
        assert_eq!(tx.sign_with(&signer), raw);
    }
}
