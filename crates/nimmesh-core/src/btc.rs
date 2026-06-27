//! # btc — the Bitcoin counterparty leg of a mesh swap (B1, `bitcoin-leg` feature)
//!
//! The BTC side of a NIM⇄BTC HTLC atomic swap, built with `rust-bitcoin`. Ported from
//! hashmark's production cross-chain HTLC (`app/src/chains/bitcoin/`) and **byte-validated against
//! `bitcoinjs-lib`** (`scripts/fixtures/btc-ref.mjs`) — exactly how `@nimiq/core` validated the NIM
//! leg. The hashlock is **`OP_SHA256`**, the *same* primitive as `nimiq::htlc` → a single 32-byte
//! `hash_root` and preimage open both legs of a swap.
//!
//! HTLC redeem script (the locked-in atomic-swap template):
//! ```text
//! OP_IF   OP_SHA256 <hash_root:32> OP_EQUALVERIFY <recipient_pk:33> OP_CHECKSIG   (claim w/ preimage)
//! OP_ELSE <cltv> OP_CHECKLOCKTIMEVERIFY OP_DROP <sender_pk:33> OP_CHECKSIG         (refund after timeout)
//! OP_ENDIF
//! ```
//! Funding target = **P2WSH** (`OP_0 SHA256(redeem)`). **Signet by default; mainnet is gated.**
//! This module is pure script/address construction (no keys, no network) — claim/refund signing
//! (BIP143) is the next B1 step; broadcasting is the B2 gateway.

use bitcoin::opcodes::all as op;
use bitcoin::script::{Builder, ScriptBuf};
use bitcoin::{Address, Network};

/// Length of a SHA-256 hashlock / preimage.
pub const HASH_LEN: usize = 32;
/// Length of a compressed secp256k1 public key.
pub const COMPRESSED_PUBKEY_LEN: usize = 33;

/// The public parameters of one BTC HTLC. `hash_root` is shared with the NIM leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BtcHtlcParams {
    /// `H = SHA-256(preimage)` — the shared cross-chain hashlock (same 32 bytes as the NIM leg).
    pub hash_root: [u8; HASH_LEN],
    /// The claimant's compressed secp256k1 pubkey (claims by revealing the preimage).
    pub recipient_pubkey: [u8; COMPRESSED_PUBKEY_LEN],
    /// The funder's compressed secp256k1 pubkey (refunds after the timeout).
    pub sender_pubkey: [u8; COMPRESSED_PUBKEY_LEN],
    /// Absolute CLTV locktime. **For a cross-chain swap, a Unix-SECONDS timestamp** (CLTV treats
    /// values ≥ 500_000_000 as Unix time) so it ladders against the NIM leg's Unix-ms timeout.
    pub cltv_locktime: i64,
}

impl BtcHtlcParams {
    /// Build the HTLC redeem script (the witnessScript a P2WSH commits to). Byte-identical to
    /// hashmark / `bitcoinjs-lib`.
    pub fn redeem_script(&self) -> ScriptBuf {
        Builder::new()
            .push_opcode(op::OP_IF)
            .push_opcode(op::OP_SHA256)
            .push_slice(self.hash_root)
            .push_opcode(op::OP_EQUALVERIFY)
            .push_slice(self.recipient_pubkey)
            .push_opcode(op::OP_CHECKSIG)
            .push_opcode(op::OP_ELSE)
            .push_int(self.cltv_locktime)
            .push_opcode(op::OP_CLTV)
            .push_opcode(op::OP_DROP)
            .push_slice(self.sender_pubkey)
            .push_opcode(op::OP_CHECKSIG)
            .push_opcode(op::OP_ENDIF)
            .into_script()
    }

    /// The P2WSH funding address (`OP_0 SHA256(redeem)`) on `network` (default signet).
    pub fn p2wsh_address(&self, network: Network) -> Address {
        Address::p2wsh(&self.redeem_script(), network)
    }

    /// The P2WSH `scriptPubKey` bytes (`0x0020 || SHA256(redeem)`).
    pub fn script_pubkey(&self, network: Network) -> ScriptBuf {
        self.p2wsh_address(network).script_pubkey()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::hex::bytes_to_hex;

    // Reference params + outputs from `scripts/fixtures/btc-ref.mjs` (bitcoinjs-lib 7.0.1,
    // signet = testnet address params). hash_root = 01..20; pubkeys are fixed 33-byte test values.
    fn ref_params() -> BtcHtlcParams {
        let mut hash_root = [0u8; HASH_LEN];
        for (i, b) in hash_root.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        let mut recipient_pubkey = [0x11u8; COMPRESSED_PUBKEY_LEN];
        recipient_pubkey[0] = 0x02;
        let mut sender_pubkey = [0x22u8; COMPRESSED_PUBKEY_LEN];
        sender_pubkey[0] = 0x03;
        BtcHtlcParams {
            hash_root,
            recipient_pubkey,
            sender_pubkey,
            cltv_locktime: 1_782_588_246,
        }
    }

    #[test]
    fn redeem_script_matches_bitcoinjs() {
        assert_eq!(
            bytes_to_hex(ref_params().redeem_script().as_bytes()),
            "63a8200102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f208821021111111111111111111111111111111111111111111111111111111111111111ac67045623406ab17521032222222222222222222222222222222222222222222222222222222222222222ac68"
        );
    }

    #[test]
    fn p2wsh_address_and_spk_match_bitcoinjs_signet() {
        let p = ref_params();
        assert_eq!(
            p.p2wsh_address(Network::Signet).to_string(),
            "tb1qk5003sapatwjjnxvv296f8qvv8wkdujx5cppul8vfj4lf26g5ylqjc9ntt"
        );
        assert_eq!(
            bytes_to_hex(p.script_pubkey(Network::Signet).as_bytes()),
            "0020b51ef8c3a1eadd294ccc628ba49c0c61dd66f246a6021e7cec4cabf4ab48a13e"
        );
    }

    #[test]
    fn cltv_is_pushed_as_minimal_le_data() {
        // 1782588246 = 0x6A402356 → minimal little-endian scriptint `5623406a` (push opcode 0x04).
        let script = ref_params().redeem_script();
        let hex = bytes_to_hex(script.as_bytes());
        assert!(hex.contains("045623406ab175")); // `04 5623406a OP_CLTV OP_DROP`
    }
}
