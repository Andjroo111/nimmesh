//! # swap_messages — build the swap wire envelopes from swap state (the engine↔wire bridge)
//!
//! Typed constructors that turn a node's swap state into the [`crate::swap_wire`] TLV envelopes the
//! mesh floods, and parse them back. The initiator's [`SwapProposal`] → `Propose`; the responder's
//! [`SwapAcceptance`] → `Accept`; a funded leg / a revealing claim → [`tx_envelope`]; a courtesy
//! cancel → [`abort`]. This is what wires the engine (terms, keys, signed txs) to the wire so a swap
//! can actually be negotiated over the mesh. Pure: no keys, no bitcoin crate — just public data.

use crate::swap::SwapTerms;
use crate::swap_wire::{
    encode_swap, SwapEnvelope, SwapLegId, SwapWireError, BTC_PUBKEY_LEN, HASH_LEN, NIM_ADDRESS_LEN,
    SWAP_ID_LEN,
};

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
}
