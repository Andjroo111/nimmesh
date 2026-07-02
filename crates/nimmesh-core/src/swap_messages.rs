//! # swap_messages — build the swap wire envelopes from swap state (the engine↔wire bridge)
//!
//! Typed constructors that turn a node's swap state into the [`crate::swap_wire`] TLV envelopes the
//! mesh floods, and parse them back. The initiator's [`SwapProposal`] → `Propose`; the responder's
//! [`SwapAcceptance`] → `Accept`; a funded leg / a revealing claim → [`tx_envelope`]; a courtesy
//! cancel → [`abort`]. This is what wires the engine (terms, keys, signed txs) to the wire so a swap
//! can actually be negotiated over the mesh. Pure: no keys, no bitcoin crate — just public data.

use ed25519_dalek::Signer;

use crate::swap::SwapTerms;
use crate::swap_wire::{
    encode_swap, SwapEnvelope, SwapLegId, SwapWireError, BTC_PUBKEY_LEN, HASH_LEN, NIM_ADDRESS_LEN,
    SWAP_ID_LEN,
};

/// Ed25519 public-key length carried by an authenticated Propose (S2 / #73) — same as an intent's.
pub const PROPOSE_PUBKEY_LEN: usize = 32;
/// Ed25519 signature length over a Propose's [`signing_bytes`](SwapProposal::signing_bytes).
pub const PROPOSE_SIG_LEN: usize = 64;

/// The initiator's proposed swap — everything a `Propose` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapProposal {
    /// The 16-byte per-swap correlator.
    pub swap_id: [u8; SWAP_ID_LEN],
    /// `H = SHA-256(secret)`, the shared hashlock.
    pub hashlock: [u8; HASH_LEN],
    /// NIM the initiator gives, in luna.
    pub give_amount: u64,
    /// BTC the initiator wants, in satoshis.
    pub take_amount: u64,
    /// `T_A` (NIM) / `T_B` (counterparty) timeouts, Unix-ms.
    pub terms: SwapTerms,
    /// The initiator's NIM refund address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// The initiator's BTC payout address (chain-agnostic bytes, e.g. a `tb1…` string).
    pub btc_address: Vec<u8>,
    /// The initiator's BTC **claimant** pubkey (needed to build the shared HTLC).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
    /// The Albatross network id for the NIM leg.
    pub network_id: u8,
}

impl SwapProposal {
    /// Build the `Propose` envelope.
    pub fn to_envelope(&self) -> SwapEnvelope {
        SwapEnvelope {
            swap_id: self.swap_id,
            hashlock: Some(self.hashlock),
            give_amount: Some(self.give_amount),
            take_amount: Some(self.take_amount),
            nim_timeout: Some(self.terms.nim_timeout),
            counterparty_timeout: Some(self.terms.counterparty_timeout),
            nim_address: Some(self.nim_address),
            counterparty_address: Some(self.btc_address.clone()),
            network_id: Some(self.network_id),
            btc_pubkey: Some(self.btc_pubkey),
            ..Default::default()
        }
    }

    /// Parse a decoded `Propose` envelope back into a proposal (`None` if a field is missing).
    pub fn from_envelope(env: &SwapEnvelope) -> Option<Self> {
        Some(SwapProposal {
            swap_id: env.swap_id,
            hashlock: env.hashlock?,
            give_amount: env.give_amount?,
            take_amount: env.take_amount?,
            terms: SwapTerms {
                nim_timeout: env.nim_timeout?,
                counterparty_timeout: env.counterparty_timeout?,
            },
            nim_address: env.nim_address?,
            btc_address: env.counterparty_address.clone()?,
            btc_pubkey: env.btc_pubkey?,
            network_id: env.network_id?,
        })
    }

    /// Encode straight to `Propose` wire bytes.
    pub fn encode(&self) -> Result<Vec<u8>, SwapWireError> {
        encode_swap(&self.to_envelope())
    }

    /// S2 / #73: the canonical bytes an authenticated Propose signs over — every **binding** term in a
    /// fixed order, so a relay cannot alter the swap_id, hashlock, amounts, timeouts, or the
    /// initiator's claim addresses in transit without invalidating the signature. Mirrors
    /// [`crate::swap_intent::SwapIntent::signing_bytes`]. (The signature itself is carried out-of-band
    /// on the wire in slice 2 and is not part of these bytes.)
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(128);
        b.extend_from_slice(&self.swap_id);
        b.extend_from_slice(&self.hashlock);
        b.extend_from_slice(&self.give_amount.to_be_bytes());
        b.extend_from_slice(&self.take_amount.to_be_bytes());
        b.extend_from_slice(&self.terms.nim_timeout.to_be_bytes());
        b.extend_from_slice(&self.terms.counterparty_timeout.to_be_bytes());
        b.extend_from_slice(&self.nim_address);
        b.extend_from_slice(&self.btc_address);
        b.extend_from_slice(&self.btc_pubkey);
        b.push(self.network_id);
        b
    }

    /// S2 / #73: sign this Propose's terms with the initiator's Ed25519 seed (the NIM account key —
    /// the same key that hashes to `nim_address`), returning `(pubkey, signature)` to carry on the
    /// wire. In production the seed stays behind the `EnclaveKey` seam; this is the pure primitive +
    /// the test/advertiser helper (mirrors [`crate::swap_intent::sign_intent`]).
    pub fn sign(&self, secret: &[u8; 32]) -> ([u8; PROPOSE_PUBKEY_LEN], [u8; PROPOSE_SIG_LEN]) {
        let sk = ed25519_dalek::SigningKey::from_bytes(secret);
        let pubkey = sk.verifying_key().to_bytes();
        let signature = sk.sign(&self.signing_bytes()).to_bytes();
        (pubkey, signature)
    }

    /// S2 / #73: verify a Propose's terms are authentic and untampered — `pubkey` must hash to this
    /// Propose's `nim_address` (so the signer controls the NIM claim address), AND `signature` must be
    /// a valid Ed25519 signature (RFC-8032 `verify_strict`) over [`signing_bytes`](Self::signing_bytes).
    /// A wrong key, a tampered field, or a junk signature returns `false`. Pure + panic-free.
    pub fn verify_signature(
        &self,
        pubkey: &[u8; PROPOSE_PUBKEY_LEN],
        signature: &[u8; PROPOSE_SIG_LEN],
    ) -> bool {
        if crate::nimiq::address::Address::from_public_key(pubkey).as_bytes() != &self.nim_address {
            return false;
        }
        let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(pubkey) else {
            return false;
        };
        vk.verify_strict(
            &self.signing_bytes(),
            &ed25519_dalek::Signature::from_bytes(signature),
        )
        .is_ok()
    }
}

/// The responder's acceptance — everything an `Accept` carries (its own addresses + BTC funder key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapAcceptance {
    /// The per-swap correlator (matches the proposal).
    pub swap_id: [u8; SWAP_ID_LEN],
    /// The responder's NIM claim address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// The responder's BTC payout/refund address (chain-agnostic bytes).
    pub btc_address: Vec<u8>,
    /// The responder's BTC **funder** pubkey (the other half of the shared HTLC).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
}

impl SwapAcceptance {
    /// Build the `Accept` envelope.
    pub fn to_envelope(&self) -> SwapEnvelope {
        SwapEnvelope {
            swap_id: self.swap_id,
            nim_address: Some(self.nim_address),
            counterparty_address: Some(self.btc_address.clone()),
            btc_pubkey: Some(self.btc_pubkey),
            ..Default::default()
        }
    }

    /// Parse a decoded `Accept` envelope (`None` if a field is missing).
    pub fn from_envelope(env: &SwapEnvelope) -> Option<Self> {
        Some(SwapAcceptance {
            swap_id: env.swap_id,
            nim_address: env.nim_address?,
            btc_address: env.counterparty_address.clone()?,
            btc_pubkey: env.btc_pubkey?,
        })
    }

    /// Encode straight to `Accept` wire bytes.
    pub fn encode(&self) -> Result<Vec<u8>, SwapWireError> {
        encode_swap(&self.to_envelope())
    }
}

/// A `FundingProof` or `PreimageReveal` envelope: a leg + its signed tx blob + the tx id. Both kinds
/// carry the same fields — a funding tx is broadcast-safe; a claim tx reveals the preimage on-chain.
pub fn tx_envelope(
    swap_id: [u8; SWAP_ID_LEN],
    leg: SwapLegId,
    tx_wire: Vec<u8>,
    tx_id: [u8; HASH_LEN],
) -> SwapEnvelope {
    SwapEnvelope {
        swap_id,
        leg: Some(leg),
        tx_wire: Some(tx_wire),
        tx_id: Some(tx_id),
        ..Default::default()
    }
}

/// A pre-funding courtesy `Abort` envelope with a 1-byte reason code.
pub fn abort(swap_id: [u8; SWAP_ID_LEN], reason: u8) -> SwapEnvelope {
    SwapEnvelope {
        swap_id,
        reason: Some(reason),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap_wire::{decode_swap, SwapKind};

    fn proposal() -> SwapProposal {
        SwapProposal {
            swap_id: [0xA1; SWAP_ID_LEN],
            hashlock: [0x7E; HASH_LEN],
            give_amount: 280_000_000,
            take_amount: 120_000,
            terms: SwapTerms {
                nim_timeout: 1_000_007_200_000,
                counterparty_timeout: 1_000_003_600_000,
            },
            nim_address: [0x4D; NIM_ADDRESS_LEN],
            btc_address: b"tb1qinitiatorpayout".to_vec(),
            btc_pubkey: {
                let mut k = [0x11; BTC_PUBKEY_LEN];
                k[0] = 0x02;
                k
            },
            network_id: 5,
        }
    }

    #[test]
    fn propose_round_trips_through_the_wire() {
        let p = proposal();
        let bytes = p.encode().unwrap();
        let decoded = decode_swap(SwapKind::Propose, &bytes).unwrap();
        assert_eq!(SwapProposal::from_envelope(&decoded), Some(p));
    }

    #[test]
    fn accept_round_trips_through_the_wire() {
        let a = SwapAcceptance {
            swap_id: [0xB2; SWAP_ID_LEN],
            nim_address: [0x5E; NIM_ADDRESS_LEN],
            btc_address: b"tb1qresponderpayout".to_vec(),
            btc_pubkey: {
                let mut k = [0x22; BTC_PUBKEY_LEN];
                k[0] = 0x03;
                k
            },
        };
        let bytes = a.encode().unwrap();
        let decoded = decode_swap(SwapKind::Accept, &bytes).unwrap();
        assert_eq!(SwapAcceptance::from_envelope(&decoded), Some(a));
    }

    #[test]
    fn pubkeys_are_exchanged_so_both_sides_can_build_the_same_htlc() {
        // The bridge: after Propose+Accept, BOTH sides hold BOTH BTC pubkeys (claimant + funder).
        let p = proposal();
        let init_seen = SwapProposal::from_envelope(
            &decode_swap(SwapKind::Propose, &p.encode().unwrap()).unwrap(),
        )
        .unwrap();
        let a = SwapAcceptance {
            swap_id: p.swap_id,
            nim_address: [0x5E; NIM_ADDRESS_LEN],
            btc_address: b"tb1qresp".to_vec(),
            btc_pubkey: {
                let mut k = [0x22; BTC_PUBKEY_LEN];
                k[0] = 0x03;
                k
            },
        };
        let resp_seen = SwapAcceptance::from_envelope(
            &decode_swap(SwapKind::Accept, &a.encode().unwrap()).unwrap(),
        )
        .unwrap();
        // claimant = initiator's pubkey (from Propose); funder = responder's (from Accept).
        assert_eq!(init_seen.btc_pubkey, p.btc_pubkey);
        assert_eq!(resp_seen.btc_pubkey, a.btc_pubkey);
    }

    #[test]
    fn funding_proof_and_reveal_carry_the_tx_blob() {
        let env = tx_envelope(
            [0xCD; SWAP_ID_LEN],
            SwapLegId::Nim,
            vec![0x11; 248],
            [0xEE; HASH_LEN],
        );
        for kind in [SwapKind::FundingProof, SwapKind::PreimageReveal] {
            let bytes = encode_swap(&env).unwrap();
            let decoded = decode_swap(kind, &bytes).unwrap();
            assert_eq!(decoded.tx_wire.as_deref(), Some(&[0x11; 248][..]));
            assert_eq!(decoded.leg, Some(SwapLegId::Nim));
        }
    }

    #[test]
    fn abort_round_trips() {
        let env = abort([0x33; SWAP_ID_LEN], 2);
        let bytes = encode_swap(&env).unwrap();
        assert_eq!(
            decode_swap(SwapKind::Abort, &bytes).unwrap().reason,
            Some(2)
        );
    }

    // --- S2 / #73: authenticated Propose terms ---------------------------------------------------

    /// A Propose whose `nim_address` is the one owned by `secret`'s Ed25519 key (so a correct
    /// signature can self-certify the address).
    fn signed_proposal(secret: &[u8; 32]) -> SwapProposal {
        let pubkey = ed25519_dalek::SigningKey::from_bytes(secret)
            .verifying_key()
            .to_bytes();
        let nim_address = *crate::nimiq::address::Address::from_public_key(&pubkey).as_bytes();
        let mut btc_pubkey = [0x11u8; BTC_PUBKEY_LEN];
        btc_pubkey[0] = 0x02;
        SwapProposal {
            swap_id: [0xA1; SWAP_ID_LEN],
            hashlock: [0x7E; HASH_LEN],
            give_amount: 100_000,
            take_amount: 50_000,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            nim_address,
            btc_address: b"tb1qinit".to_vec(),
            btc_pubkey,
            network_id: 5,
        }
    }

    #[test]
    fn a_signed_proposal_verifies() {
        let secret = [7u8; 32];
        let p = signed_proposal(&secret);
        let (pubkey, sig) = p.sign(&secret);
        assert!(p.verify_signature(&pubkey, &sig));
    }

    #[test]
    fn a_tampered_amount_fails_verification() {
        // A relay rewrites the terms in transit — the signature no longer covers them.
        let secret = [7u8; 32];
        let p = signed_proposal(&secret);
        let (pubkey, sig) = p.sign(&secret);
        let mut tampered = p.clone();
        tampered.give_amount = 1;
        assert!(!tampered.verify_signature(&pubkey, &sig));
    }

    #[test]
    fn a_tampered_hashlock_or_timeout_fails_verification() {
        let secret = [7u8; 32];
        let p = signed_proposal(&secret);
        let (pubkey, sig) = p.sign(&secret);
        let mut h = p.clone();
        h.hashlock = [0xFF; HASH_LEN];
        assert!(!h.verify_signature(&pubkey, &sig));
        let mut t = p.clone();
        t.terms.counterparty_timeout = 9_999;
        assert!(!t.verify_signature(&pubkey, &sig));
    }

    #[test]
    fn a_junk_signature_fails() {
        let p = signed_proposal(&[7u8; 32]);
        let (pubkey, _) = p.sign(&[7u8; 32]);
        assert!(!p.verify_signature(&pubkey, &[0u8; PROPOSE_SIG_LEN]));
    }

    #[test]
    fn a_relay_that_re_signs_with_its_own_key_is_rejected() {
        // The self-certifying bind: a MITM can produce a valid signature over tampered terms, but only
        // under ITS key — which does not hash to the Propose's nim_address, so the responder rejects it.
        let p = signed_proposal(&[7u8; 32]);
        let (atk_pubkey, atk_sig) = p.sign(&[9u8; 32]);
        assert!(!p.verify_signature(&atk_pubkey, &atk_sig));
    }
}
