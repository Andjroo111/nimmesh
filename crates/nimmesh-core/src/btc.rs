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
//! Broadcasting is the B2 gateway. Signing goes through the [`BtcEnclaveKey`] seam (the BTC mirror
//! of [`crate::nimiq::signer::EnclaveKey`]): the secp256k1 secret stays behind the trait — only a
//! 33-byte public key and a DER signature over the BIP143 sighash ever leave it, so a native Secure
//! Enclave / Keystore impl drives on-device signing. The raw-key `claim_tx`/`refund_tx` are thin
//! wrappers over an in-memory key for tests/dev.

use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all as op;
use bitcoin::script::{Builder, ScriptBuf};
use bitcoin::secp256k1::{ecdsa, All, Message, PublicKey, Secp256k1, SecretKey};
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
        let key = InMemoryBtcEnclaveKey::from_secret(recipient_sk)?;
        self.claim_tx_with_key(funded, preimage, dest_spk, payout_sat, &key)
    }

    /// Like [`claim_tx`](Self::claim_tx) but signs through the opaque [`BtcEnclaveKey`] seam — the
    /// secp256k1 secret never enters this crate, only a signature comes back. This is the on-device
    /// path (a native Secure Enclave / Keystore impl); `claim_tx` is the in-memory wrapper for dev.
    pub fn claim_tx_with_key(
        &self,
        funded: &FundedHtlc,
        preimage: &[u8; HASH_LEN],
        dest_spk: ScriptBuf,
        payout_sat: u64,
        key: &dyn BtcEnclaveKey,
    ) -> Result<Vec<u8>, BtcError> {
        // claim: nLockTime 0, final sequence; branch items = [preimage, 0x01 (IF-true)].
        self.sign_spend(
            funded,
            dest_spk,
            payout_sat,
            0,
            0xffff_ffff,
            &[preimage, &[0x01]],
            key,
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
        let key = InMemoryBtcEnclaveKey::from_secret(sender_sk)?;
        self.refund_tx_with_key(funded, dest_spk, payout_sat, &key)
    }

    /// Like [`refund_tx`](Self::refund_tx) but signs through the [`BtcEnclaveKey`] seam (on-device).
    pub fn refund_tx_with_key(
        &self,
        funded: &FundedHtlc,
        dest_spk: ScriptBuf,
        payout_sat: u64,
        key: &dyn BtcEnclaveKey,
    ) -> Result<Vec<u8>, BtcError> {
        let locktime = u32::try_from(self.cltv_locktime).map_err(|_| BtcError::BadLocktime)?;
        // refund: nLockTime = cltv, sequence 0xfffffffe (non-final, enables CLTV); branch = [<empty>].
        self.sign_spend(
            funded,
            dest_spk,
            payout_sat,
            locktime,
            0xffff_fffe,
            &[&[]],
            key,
        )
    }

    /// Build a 1-in/1-out spend of the funded HTLC, sign input 0 over the BIP143 segwit-v0 sighash
    /// **through the [`BtcEnclaveKey`] seam** (the secret never enters this function), and assemble
    /// the witness `[sig, <branch items…>, redeemScript]`.
    #[allow(clippy::too_many_arguments)]
    fn sign_spend(
        &self,
        funded: &FundedHtlc,
        dest_spk: ScriptBuf,
        payout_sat: u64,
        locktime: u32,
        sequence: u32,
        branch: &[&[u8]],
        key: &dyn BtcEnclaveKey,
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
        // Sign the 32-byte sighash behind the key seam; only the DER signature comes back.
        let der = key.sign_sighash(sighash.to_byte_array().to_vec());
        // Re-normalize to low-S so even a native enclave that returns a high-S signature can't
        // produce a non-standard (relay-rejected) tx. For the in-memory key this is already
        // canonical, so the bytes stay byte-identical to bitcoinjs-lib.
        let mut sig = ecdsa::Signature::from_der(&der).map_err(|_| BtcError::BadSignature)?;
        sig.normalize_s();
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

// --- BtcEnclaveKey: the native secp256k1 key seam (the BTC mirror of nimiq::signer::EnclaveKey) ---

/// The opaque native secp256k1 key seam for the Bitcoin leg. The Secure Enclave / Android Keystore
/// implements this so the **secp256k1 secret never crosses the FFI boundary**: only the 33-byte
/// compressed public key leaves (to build the HTLC redeem script + the P2WPKH payout address) and a
/// DER-encoded ECDSA signature over the 32-byte BIP143 sighash leaves. [`BtcHtlcParams::sign_spend`]
/// re-normalizes the returned signature to low-S, so a native impl cannot produce a non-standard
/// (relay-rejected) signature.
///
/// `// NATIVE:` on iOS a real impl signs via a non-exportable secp256k1 `SecKey`; on Android via a
/// hardware-backed `KeyStore` entry. One wallet seed derives **both** this BTC key (BIP32/BIP84) and
/// the NIM [`EnclaveKey`](crate::nimiq::signer::EnclaveKey) — that derivation is the native layer's
/// job; the core only ever sees the opaque handle. Verified on-device later; here we model the shape.
#[uniffi::export(with_foreign)]
pub trait BtcEnclaveKey: Send + Sync {
    /// The 33-byte compressed secp256k1 public key (safe to leak; goes into the redeem script and
    /// the P2WPKH payout address).
    fn public_key(&self) -> Vec<u8>;
    /// Sign the 32-byte BIP143 segwit-v0 sighash digest, returning the DER-encoded ECDSA signature
    /// (no sighash-type byte — the tx assembler appends it). MUST be over secp256k1; SHOULD be
    /// RFC6979-deterministic + low-S (the assembler normalizes low-S regardless).
    fn sign_sighash(&self, sighash: Vec<u8>) -> Vec<u8>;
}

/// In-process reference [`BtcEnclaveKey`] for tests/dev ONLY. **Not** FFI-exported: a secret-bearing
/// constructor must never appear on the UniFFI surface (the seed never crosses FFI). Production uses
/// a native enclave-backed impl. Signs with deterministic RFC6979 + low-S, so its output is
/// byte-identical to `bitcoinjs-lib` and consensus-canonical.
pub struct InMemoryBtcEnclaveKey {
    secret: SecretKey,
    secp: Secp256k1<All>,
}

impl InMemoryBtcEnclaveKey {
    /// Build from a 32-byte secp256k1 secret (Rust-side test/dev only). Errors if the bytes are not
    /// a valid secp256k1 scalar.
    pub fn from_secret(secret: &[u8; 32]) -> Result<Self, BtcError> {
        Ok(InMemoryBtcEnclaveKey {
            secret: SecretKey::from_slice(secret).map_err(|_| BtcError::BadKey)?,
            secp: Secp256k1::new(),
        })
    }
}

impl BtcEnclaveKey for InMemoryBtcEnclaveKey {
    fn public_key(&self) -> Vec<u8> {
        PublicKey::from_secret_key(&self.secp, &self.secret)
            .serialize()
            .to_vec()
    }

    fn sign_sighash(&self, sighash: Vec<u8>) -> Vec<u8> {
        let digest: [u8; HASH_LEN] = match sighash.as_slice().try_into() {
            Ok(d) => d,
            Err(_) => return Vec::new(), // wrong-length digest → empty sig → assembler rejects
        };
        self.secp
            .sign_ecdsa(&Message::from_digest(digest), &self.secret)
            .serialize_der()
            .to_vec()
    }
}

/// Extract the revealed preimage from a broadcast HTLC **claim** tx — witness item `[1]` of input 0
/// (the claim witness is `[sig, preimage, 0x01, redeemScript]`). This is how the cross-chain
/// counterparty learns `S` by watching the chain: once the initiator claims the BTC leg, `S` is
/// public here and unlocks the NIM leg. Returns `None` if the bytes don't deserialize, there is no
/// input, or item `[1]` is not a 32-byte value (e.g. a refund tx, whose item `[1]` is empty).
pub fn extract_preimage(claim_tx: &[u8]) -> Option<[u8; HASH_LEN]> {
    let tx: Transaction = bitcoin::consensus::encode::deserialize(claim_tx).ok()?;
    let item = tx.input.first()?.witness.iter().nth(1)?;
    item.try_into().ok()
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
    /// The key seam returned a signature that is not parseable DER (corrupt / wrong-length digest).
    BadSignature,
    /// The fee is greater than the funded value — the spend would have a negative/dust output.
    FeeExceedsValue,
}

impl std::fmt::Display for BtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtcError::BadKey => write!(f, "invalid secp256k1 secret key"),
            BtcError::BadLocktime => write!(f, "CLTV locktime does not fit a u32"),
            BtcError::Sighash => write!(f, "could not compute the BIP143 sighash"),
            BtcError::BadSignature => write!(f, "key seam returned an unparseable signature"),
            BtcError::FeeExceedsValue => write!(f, "fee exceeds the funded value"),
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

    // The on-device key seam: driving the spend through `BtcEnclaveKey` must produce byte-identical
    // bytes to the raw-key path — proving the seam is faithful (same on-chain tx, secret behind FFI).
    #[test]
    fn enclave_seam_signs_byte_identically_to_raw_key() {
        let recipient = InMemoryBtcEnclaveKey::from_secret(&[0x11u8; 32]).unwrap();
        let sender = InMemoryBtcEnclaveKey::from_secret(&[0x22u8; 32]).unwrap();
        // The seam exposes exactly the compressed pubkeys baked into the reference redeem script.
        assert_eq!(
            bytes_to_hex(&recipient.public_key()),
            "034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa"
        );
        assert_eq!(
            bytes_to_hex(&sender.public_key()),
            "02466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27"
        );

        let preimage: [u8; 32] = std::array::from_fn(|i| (i + 1) as u8);
        // claim through the seam == claim with the raw key (which is itself pinned to bitcoinjs).
        let via_seam = spend_params()
            .claim_tx_with_key(&fixture_funded(), &preimage, dest_spk(), 99_000, &recipient)
            .unwrap();
        let via_raw = spend_params()
            .claim_tx(
                &fixture_funded(),
                &preimage,
                dest_spk(),
                99_000,
                &[0x11u8; 32],
            )
            .unwrap();
        assert_eq!(via_seam, via_raw);
        // refund through the seam == refund with the raw key.
        let refund_seam = spend_params()
            .refund_tx_with_key(&fixture_funded(), dest_spk(), 99_000, &sender)
            .unwrap();
        let refund_raw = spend_params()
            .refund_tx(&fixture_funded(), dest_spk(), 99_000, &[0x22u8; 32])
            .unwrap();
        assert_eq!(refund_seam, refund_raw);
    }

    #[test]
    fn extract_preimage_reads_s_from_a_claim_and_none_from_a_refund() {
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
        // The counterparty recovers exactly S from the claim witness.
        assert_eq!(extract_preimage(&claim), Some(preimage));
        // A refund reveals nothing (witness item [1] is empty, not a 32-byte preimage).
        let refund = spend_params()
            .refund_tx(&fixture_funded(), dest_spk(), 99_000, &[0x22u8; 32])
            .unwrap();
        assert_eq!(extract_preimage(&refund), None);
        // Garbage in → None, never a panic.
        assert_eq!(extract_preimage(&[0x00, 0x01, 0x02]), None);
    }

    #[test]
    fn enclave_key_rejects_a_bad_secret() {
        // The all-zero scalar is not a valid secp256k1 key. (Match rather than `unwrap_err` so the
        // secret-holding key type never needs a `Debug` impl that could leak the seed.)
        match InMemoryBtcEnclaveKey::from_secret(&[0u8; 32]) {
            Err(e) => assert_eq!(e, BtcError::BadKey),
            Ok(_) => panic!("all-zero scalar must be rejected"),
        }
    }
}
