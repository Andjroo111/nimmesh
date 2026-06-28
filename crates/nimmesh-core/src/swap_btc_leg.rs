//! # swap_btc_leg — the real Bitcoin swap leg (mesh swap, `bitcoin-leg` feature)
//!
//! The BTC counterparty leg as a first-class swap component — the BTC-native analog of
//! [`crate::swap_builder::NimiqLeg`]. It wraps [`crate::btc::BtcHtlcParams`] (the byte-validated,
//! live-proven P2WSH HTLC) and signs through the [`crate::btc::BtcEnclaveKey`] seam, so the
//! secp256k1 secret never leaves the enclave.
//!
//! This **supersedes the BTC half of the [`crate::swap_builder::BitcoinLeg`] gated stub**. BTC's
//! UTXO model does not fit the NIM-shaped [`crate::swap_builder::LegBuilder`] trait:
//!
//! - there is no `validity_start_height` to anchor — a BTC spend is anchored by its input outpoint
//!   and `nLockTime`/CLTV, not a block height;
//! - **funding is a wallet payment to a P2WSH address**, not a leg-built "creation" tx — the leg
//!   can hand back the address to pay, but it cannot build the funding tx without the funder's UTXOs.
//!
//! So the BTC leg keeps its own chain-natural interface, which [`crate::swap_engine::SwapEngine`]
//! drives: `htlc_address()` to fund/watch, `build_claim()` for the initiator (reveals the preimage),
//! `build_refund()` for the funder (after the CLTV). Testnet/signet only; mainnet stays gated.

use std::sync::Arc;

use bitcoin::{Address, Network, ScriptBuf};

use crate::btc::{
    BtcEnclaveKey, BtcError, BtcHtlcParams, FundedHtlc, COMPRESSED_PUBKEY_LEN, HASH_LEN,
};

/// One node's view of the Bitcoin HTLC leg of a swap. Holds the public HTLC parameters (which fully
/// determine the P2WSH address both sides can derive), this node's opaque signing key, where its
/// claimed/refunded BTC should go, and a flat fee. The funder (responder) pays the P2WSH; the
/// claimant (initiator) spends it by revealing the preimage; either side can be refunded after the
/// CLTV — but a given `BtcSwapLeg`'s `key` must match the role it builds a spend for.
pub struct BtcSwapLeg {
    htlc: BtcHtlcParams,
    network: Network,
    key: Arc<dyn BtcEnclaveKey>,
    payout_spk: ScriptBuf,
    fee_sat: u64,
}

impl BtcSwapLeg {
    /// Build a BTC swap leg from the agreed terms + this node's key.
    ///
    /// - `hashlock` — the shared `H = SHA-256(secret)` (the same 32 bytes as the NIM leg).
    /// - `claimant_pubkey` — the **initiator's** compressed secp256k1 pubkey (claims with the preimage).
    /// - `funder_pubkey` — the **responder's** compressed secp256k1 pubkey (refunds after the CLTV).
    /// - `cltv_locktime` — `T_B`, a Unix-**seconds** timestamp (CLTV treats `≥ 500_000_000` as time).
    /// - `key` — THIS node's BTC enclave key. It must be the claimant to build a claim, the funder to
    ///   build a refund (the chain enforces this anyway; mismatch yields an invalid signature).
    /// - `payout_spk` — where this node's claimed/refunded BTC lands (its own P2WPKH scriptPubKey).
    /// - `fee_sat` — the flat fee deducted from the HTLC value on a claim/refund.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hashlock: [u8; HASH_LEN],
        claimant_pubkey: [u8; COMPRESSED_PUBKEY_LEN],
        funder_pubkey: [u8; COMPRESSED_PUBKEY_LEN],
        cltv_locktime: i64,
        network: Network,
        key: Arc<dyn BtcEnclaveKey>,
        payout_spk: ScriptBuf,
        fee_sat: u64,
    ) -> Self {
        BtcSwapLeg {
            htlc: BtcHtlcParams {
                hash_root: hashlock,
                recipient_pubkey: claimant_pubkey,
                sender_pubkey: funder_pubkey,
                cltv_locktime,
            },
            network,
            key,
            payout_spk,
            fee_sat,
        }
    }

    /// The P2WSH HTLC address the funder pays and the watcher watches.
    pub fn htlc_address(&self) -> Address {
        self.htlc.p2wsh_address(self.network)
    }

    /// The P2WSH `scriptPubKey` — used to find the funding output among a tx's outputs.
    pub fn script_pubkey(&self) -> ScriptBuf {
        self.htlc.script_pubkey(self.network)
    }

    /// The leg's public HTLC parameters (hashlock, both pubkeys, CLTV).
    pub fn params(&self) -> &BtcHtlcParams {
        &self.htlc
    }

    /// Build the **claim** tx (initiator path): spend the funded HTLC by revealing `preimage`, paying
    /// `value − fee` to this node's payout spk. Signs through the enclave seam (this node's key must
    /// be the claimant). The witness puts the preimage on-chain — that is how the counterparty learns
    /// the secret to claim the NIM leg.
    pub fn build_claim(
        &self,
        funded: &FundedHtlc,
        preimage: &[u8; HASH_LEN],
    ) -> Result<Vec<u8>, BtcError> {
        let payout = self.payout(funded)?;
        self.htlc.claim_tx_with_key(
            funded,
            preimage,
            self.payout_spk.clone(),
            payout,
            self.key.as_ref(),
        )
    }

    /// Build the **refund** tx (funder path): reclaim the funded HTLC after the CLTV, paying
    /// `value − fee` to this node's payout spk. Signs through the enclave seam (this node's key must
    /// be the funder). Anchored by `nLockTime = cltv` + a non-final sequence.
    pub fn build_refund(&self, funded: &FundedHtlc) -> Result<Vec<u8>, BtcError> {
        let payout = self.payout(funded)?;
        self.htlc
            .refund_tx_with_key(funded, self.payout_spk.clone(), payout, self.key.as_ref())
    }

    /// The payout amount after the flat fee, erroring if the fee would exceed the funded value.
    fn payout(&self, funded: &FundedHtlc) -> Result<u64, BtcError> {
        funded
            .value_sat
            .checked_sub(self.fee_sat)
            .ok_or(BtcError::FeeExceedsValue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btc::InMemoryBtcEnclaveKey;
    use crate::nimiq::hex::{bytes_to_hex, hex_to_bytes};
    use bitcoin::Txid;
    use std::str::FromStr;

    fn h32(s: &str) -> [u8; 32] {
        hex_to_bytes(s).unwrap().try_into().unwrap()
    }
    fn h33(s: &str) -> [u8; 33] {
        hex_to_bytes(s).unwrap().try_into().unwrap()
    }

    // The same proven keypairs/params as `btc::tests::spend_params` (recipient sk = 0x11×32, funder
    // sk = 0x22×32, preimage = 01..20, hashRoot = SHA-256(preimage), value 100_000, fee 1_000 →
    // payout 99_000, dest = tb1qw508…). Reproducing those EXACT reference bytes THROUGH the leg
    // proves the leg builds the bytes already byte-validated vs bitcoinjs-lib AND live-confirmed.
    const HASHLOCK: &str = "ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c9";
    const CLAIMANT_PK: &str = "034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa";
    const FUNDER_PK: &str = "02466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27";
    const CLTV: i64 = 1_782_588_246;

    fn dest_spk() -> ScriptBuf {
        Address::from_str("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx")
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    fn funded() -> FundedHtlc {
        FundedHtlc {
            txid: Txid::from_str(&"11".repeat(32)).unwrap(),
            vout: 0,
            value_sat: 100_000,
        }
    }

    fn claim_leg() -> BtcSwapLeg {
        BtcSwapLeg::new(
            h32(HASHLOCK),
            h33(CLAIMANT_PK),
            h33(FUNDER_PK),
            CLTV,
            Network::Signet,
            Arc::new(InMemoryBtcEnclaveKey::from_secret(&[0x11u8; 32]).unwrap()), // claimant key
            dest_spk(),
            1_000,
        )
    }

    fn refund_leg() -> BtcSwapLeg {
        BtcSwapLeg::new(
            h32(HASHLOCK),
            h33(CLAIMANT_PK),
            h33(FUNDER_PK),
            CLTV,
            Network::Signet,
            Arc::new(InMemoryBtcEnclaveKey::from_secret(&[0x22u8; 32]).unwrap()), // funder key
            dest_spk(),
            1_000,
        )
    }

    #[test]
    fn leg_claim_matches_the_proven_reference_bytes() {
        let preimage: [u8; 32] = std::array::from_fn(|i| (i + 1) as u8);
        let claim = claim_leg().build_claim(&funded(), &preimage).unwrap();
        assert_eq!(bytes_to_hex(&claim), "0200000000010111111111111111111111111111111111111111111111111111111111111111110000000000ffffffff01b882010000000000160014751e76e8199196d454941c45d1b3a323f1433bd604473044022037c25c05c943f73e5e48dfc6b7272dbdbda924842627b6e3a463f864c79289c2022039b08be7281f1d1074e4586442a96fc2caeb817102f115addb455ce3700d7e5801200102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f2001017363a820ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c98821034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aaac67045623406ab1752102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27ac6800000000");
    }

    #[test]
    fn leg_refund_matches_the_proven_reference_bytes() {
        let refund = refund_leg().build_refund(&funded()).unwrap();
        assert_eq!(bytes_to_hex(&refund), "0200000000010111111111111111111111111111111111111111111111111111111111111111110000000000feffffff01b882010000000000160014751e76e8199196d454941c45d1b3a323f1433bd6034830450221009191bae0530e6717c382bb066969a19443d071b06e31cb40a03bf39784bfbb3c02205615e27fcb79ff17838de55053eb0bfb0179b88d5047db740f8d7b650e8db55701007363a820ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c98821034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aaac67045623406ab1752102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f27ac685623406a");
    }

    #[test]
    fn htlc_address_is_a_stable_p2wsh_both_sides_derive() {
        // Both legs (claimant + funder views) derive the SAME P2WSH address from the public params.
        let a = claim_leg().htlc_address();
        let b = refund_leg().htlc_address();
        assert_eq!(a, b);
        assert!(a.to_string().starts_with("tb1q")); // bech32 segwit (testnet/signet)
                                                    // scriptPubKey is OP_0 <32-byte witness program>.
        let spk = claim_leg().script_pubkey();
        let spk_hex = bytes_to_hex(spk.as_bytes());
        assert!(spk_hex.starts_with("0020"));
        assert_eq!(spk.len(), 34);
    }

    #[test]
    fn fee_exceeding_value_is_rejected_not_a_negative_output() {
        let leg = BtcSwapLeg::new(
            h32(HASHLOCK),
            h33(CLAIMANT_PK),
            h33(FUNDER_PK),
            CLTV,
            Network::Signet,
            Arc::new(InMemoryBtcEnclaveKey::from_secret(&[0x11u8; 32]).unwrap()),
            dest_spk(),
            200_000, // fee > the 100_000 funded value
        );
        let preimage: [u8; 32] = std::array::from_fn(|i| (i + 1) as u8);
        assert_eq!(
            leg.build_claim(&funded(), &preimage),
            Err(BtcError::FeeExceedsValue)
        );
    }
}
