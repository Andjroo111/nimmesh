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
//! Builds the HTLC script + P2WSH address **and** the BIP143-signed claim/refund txs (the full
//! signed bytes match `bitcoinjs-lib` exactly — both use deterministic RFC6979 + low-S ECDSA).
//! Broadcasting is the B2 gateway; the on-device signing seam (vs the in-process key here) is a
//! follow-up mirroring `nimiq::signer`'s `EnclaveKey`.

use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all as op;
use bitcoin::script::{Builder, ScriptBuf};
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Address, Amount, Network, OutPoint, Sequence, Transaction, TxIn, TxOut, Witness};

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

    /// The **claim** tx: spend the funded HTLC via the preimage branch (`OP_IF`), paying
    /// `payout_sat` to `dest_spk`. Signed (BIP143, SIGHASH_ALL) with the recipient's key. The
    /// witness `[sig, preimage, 0x01, redeem]` reveals the preimage on-chain. Returns the full
    /// signed tx bytes (consensus-serialized).
    pub fn claim_tx(
        &self,
        funded: &FundedHtlc,
        preimage: &[u8; HASH_LEN],
        dest_spk: ScriptBuf,
        payout_sat: u64,
        recipient_sk: &[u8; 32],
    ) -> Result<Vec<u8>, BtcError> {
        let sk = SecretKey::from_slice(recipient_sk).map_err(|_| BtcError::BadKey)?;
        // claim: nLockTime 0, final sequence; branch items = [preimage, 0x01 (IF-true)].
        self.sign_spend(
            funded,
            dest_spk,
            payout_sat,
            0,
            0xffff_ffff,
            &[preimage, &[0x01]],
            &sk,
        )
    }

    /// The **refund** tx: spend via the timeout branch (`OP_ELSE`) after the CLTV, paying
    /// `payout_sat` to `dest_spk`. Signed with the sender's key. The witness `[sig, <empty>, redeem]`
    /// takes the false branch; `nLockTime = cltv_locktime` and a non-final sequence satisfy CLTV.
    pub fn refund_tx(
        &self,
        funded: &FundedHtlc,
        dest_spk: ScriptBuf,
        payout_sat: u64,
        sender_sk: &[u8; 32],
    ) -> Result<Vec<u8>, BtcError> {
        let sk = SecretKey::from_slice(sender_sk).map_err(|_| BtcError::BadKey)?;
        let locktime = u32::try_from(self.cltv_locktime).map_err(|_| BtcError::BadLocktime)?;
        // refund: nLockTime = cltv, sequence 0xfffffffe (non-final, enables CLTV); branch = [<empty>].
        self.sign_spend(
            funded,
            dest_spk,
            payout_sat,
            locktime,
            0xffff_fffe,
            &[&[]],
            &sk,
        )
    }

    /// Build a 1-in/1-out spend of the funded HTLC, sign input 0 over the BIP143 segwit-v0 sighash,
    /// and assemble the witness `[sig, <branch items…>, redeemScript]`.
    #[allow(clippy::too_many_arguments)]
    fn sign_spend(
        &self,
        funded: &FundedHtlc,
        dest_spk: ScriptBuf,
        payout_sat: u64,
        locktime: u32,
        sequence: u32,
        branch: &[&[u8]],
        sk: &SecretKey,
    ) -> Result<Vec<u8>, BtcError> {
        let redeem = self.redeem_script();
        let mut tx = Transaction {
            version: Version(2),
            lock_time: LockTime::from_consensus(locktime),
            input: vec![TxIn {
                previous_output: OutPoint::new(funded.txid, funded.vout),
                script_sig: ScriptBuf::new(),
                sequence: Sequence(sequence),
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(payout_sat),
                script_pubkey: dest_spk,
            }],
        };
        let sighash = SighashCache::new(&tx)
            .p2wsh_signature_hash(
                0,
                &redeem,
                Amount::from_sat(funded.value_sat),
                EcdsaSighashType::All,
            )
            .map_err(|_| BtcError::Sighash)?;
        let secp = Secp256k1::new();
        let sig = secp.sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), sk);
        let mut sig_bytes = sig.serialize_der().to_vec();
        sig_bytes.push(EcdsaSighashType::All as u8); // append the sighash-type byte
        let mut witness = Witness::new();
        witness.push(sig_bytes);
        for item in branch {
            witness.push(item);
        }
        witness.push(redeem.as_bytes());
        tx.input[0].witness = witness;
        Ok(bitcoin::consensus::encode::serialize(&tx))
    }
}

/// A funded BTC HTLC the claim/refund spend from: which output (`txid:vout`) and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundedHtlc {
    /// The funding transaction id.
    pub txid: bitcoin::Txid,
    /// The funding output index.
    pub vout: u32,
    /// The funded value in satoshis (BIP143 signs over this).
    pub value_sat: u64,
}

/// A failure building a BTC spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtcError {
    /// The secret key bytes were not a valid secp256k1 key.
    BadKey,
    /// The CLTV locktime did not fit a u32.
    BadLocktime,
    /// The BIP143 sighash could not be computed.
    Sighash,
}

impl std::fmt::Display for BtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtcError::BadKey => write!(f, "invalid secp256k1 secret key"),
            BtcError::BadLocktime => write!(f, "CLTV locktime does not fit a u32"),
            BtcError::Sighash => write!(f, "could not compute the BIP143 sighash"),
        }
    }
}

impl std::error::Error for BtcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};
    use bitcoin::Txid;
    use std::str::FromStr;

    fn h32(s: &str) -> [u8; 32] {
        hex_to_bytes(s).unwrap().try_into().unwrap()
    }
    fn h33(s: &str) -> [u8; 33] {
        hex_to_bytes(s).unwrap().try_into().unwrap()
    }

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

    // Reference SIGNED claim/refund txs from `scripts/fixtures/btc-spend-ref.mjs` (real keypairs:
    // recipient sk = 0x11×32, sender sk = 0x22×32; preimage = 01..20; hashRoot = SHA-256(preimage)).
    // Both libs use deterministic RFC6979 ECDSA + low-S, so the full signed bytes are identical.
    fn spend_params() -> BtcHtlcParams {
        BtcHtlcParams {
            hash_root: h32("ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c9"),
            recipient_pubkey: h33(
                "034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa",
            ),
            sender_pubkey: h33(
                "02466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27",
            ),
            cltv_locktime: 1_782_588_246,
        }
    }

    fn fixture_funded() -> FundedHtlc {
        FundedHtlc {
            txid: Txid::from_str(&"11".repeat(32)).unwrap(),
            vout: 0,
            value_sat: 100_000,
        }
    }

    fn dest_spk() -> ScriptBuf {
        Address::from_str("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    #[test]
    fn signed_claim_tx_matches_bitcoinjs() {
        let preimage: [u8; 32] = std::array::from_fn(|i| (i + 1) as u8);
        let claim = spend_params()
            .claim_tx(
                &fixture_funded(),
                &preimage,
                dest_spk(),
                99_000,
                &[0x11u8; 32],
            )
            .unwrap();
        assert_eq!(bytes_to_hex(&claim), "0200000000010111111111111111111111111111111111111111111111111111111111111111110000000000ffffffff01b882010000000000160014751e76e8199196d454941c45d1b3a323f1433bd604473044022037c25c05c943f73e5e48dfc6b7272dbdbda924842627b6e3a463f864c79289c2022039b08be7281f1d1074e4586442a96fc2caeb817102f115addb455ce3700d7e5801200102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f2001017363a820ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c98821034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aaac67045623406ab1752102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27ac6800000000");
    }

    #[test]
    fn signed_refund_tx_matches_bitcoinjs() {
        let refund = spend_params()
            .refund_tx(&fixture_funded(), dest_spk(), 99_000, &[0x22u8; 32])
            .unwrap();
        assert_eq!(bytes_to_hex(&refund), "0200000000010111111111111111111111111111111111111111111111111111111111111111110000000000feffffff01b882010000000000160014751e76e8199196d454941c45d1b3a323f1433bd6034830450221009191bae0530e6717c382bb066969a19443d071b06e31cb40a03bf39784bfbb3c02205615e27fcb79ff17838de55053eb0bfb0179b88d5047db740f8d7b650e8db55701007363a820ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c98821034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aaac67045623406ab1752102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27ac685623406a");
    }
}
