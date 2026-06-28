//! # swap_session — route incoming swap packets to the right coordinator (the node↔coordinator glue)
//!
//! A node may have several swaps in flight. [`SwapSession`] owns one [`SwapCoordinator`] per swap,
//! keyed by `swap_id`, and routes each incoming swap packet to the right one — creating a responder
//! coordinator when a fresh `Propose` arrives, and returning the outgoing envelopes to flood. It is
//! the thin layer a [`crate::node::MeshNode`] sits behind: packets in, coordinator dispatch,
//! envelopes out. The chain actions a coordinator implies (fund / claim / extract `S`) stay with the
//! node — it reaches the coordinator via [`coordinator`](SwapSession::coordinator). Pure: no keys,
//! no bitcoin.

use std::collections::HashMap;

use crate::packet::MessageType;
use crate::swap::LadderParams;
use crate::swap_coordinator::{CoordError, SwapContext, SwapCoordinator};
use crate::swap_messages::SwapProposal;
use crate::swap_wire::{
    decode_swap, encode_swap, SwapKind, SwapWireError, BTC_PUBKEY_LEN, NIM_ADDRESS_LEN, SWAP_ID_LEN,
};

/// This node's fixed identity (everything a coordinator needs that isn't learned from the peer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// This node's NIM address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// This node's BTC payout address bytes.
    pub btc_address: Vec<u8>,
    /// This node's BTC pubkey (33 bytes).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
}

/// A routing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The swap envelope didn't decode.
    Wire(SwapWireError),
    /// The target coordinator rejected the message.
    Coord(CoordError),
    /// A message arrived for a `swap_id` we have no coordinator for.
    UnknownSwap,
    /// A `Propose` was missing fields needed to set up the responder.
    BadPropose,
}

impl From<SwapWireError> for SessionError {
    fn from(e: SwapWireError) -> Self {
        SessionError::Wire(e)
    }
}
impl From<CoordError> for SessionError {
    fn from(e: CoordError) -> Self {
        SessionError::Coord(e)
    }
}

/// One node's view of all its in-flight swaps.
pub struct SwapSession {
    identity: NodeIdentity,
    ladder: LadderParams,
    coordinators: HashMap<[u8; SWAP_ID_LEN], SwapCoordinator>,
}

impl SwapSession {
    /// A session for `identity` with the given safety ladder.
    pub fn new(identity: NodeIdentity, ladder: LadderParams) -> Self {
        SwapSession {
            identity,
            ladder,
            coordinators: HashMap::new(),
        }
    }

    /// Register an initiator coordinator this node started, so its `Accept`/`FundingProof` route to it.
    pub fn add_initiator(&mut self, swap_id: [u8; SWAP_ID_LEN], coordinator: SwapCoordinator) {
        self.coordinators.insert(swap_id, coordinator);
    }

    /// The coordinator for a swap, for the node to drive its chain actions (fund / claim / reveal).
    pub fn coordinator(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Option<&mut SwapCoordinator> {
        self.coordinators.get_mut(swap_id)
    }

    /// The number of in-flight swaps.
    pub fn len(&self) -> usize {
        self.coordinators.len()
    }

    /// Whether there are no in-flight swaps.
    pub fn is_empty(&self) -> bool {
        self.coordinators.is_empty()
    }

    /// Route an incoming swap packet (`kind` from the [`MessageType`], `payload` the TLV bytes) to
    /// the right coordinator. Returns outgoing `(MessageType, bytes)` envelopes to flood. A fresh
    /// `Propose` spins up a responder coordinator; `Accept`/`FundingProof` dispatch to the existing
    /// one; `Abort` drops it. `PreimageReveal` is routed (the node extracts `S` and drives the
    /// claim itself via [`coordinator`](Self::coordinator)).
    pub fn on_message(
        &mut self,
        kind: SwapKind,
        payload: &[u8],
        head: u64,
    ) -> Result<Vec<(MessageType, Vec<u8>)>, SessionError> {
        let env = decode_swap(kind, payload)?;
        let swap_id = env.swap_id;
        match kind {
            SwapKind::Propose => {
                let p = SwapProposal::from_envelope(&env).ok_or(SessionError::BadPropose)?;
                let ctx = SwapContext {
                    swap_id,
                    terms: p.terms,
                    hashlock: p.hashlock,
                    nim_address: self.identity.nim_address,
                    btc_address: self.identity.btc_address.clone(),
                    btc_pubkey: self.identity.btc_pubkey,
                    give_amount: p.give_amount,
                    take_amount: p.take_amount,
                    network_id: p.network_id,
                };
                let mut coord = SwapCoordinator::new_responder(ctx, self.ladder);
                let accept = coord.recv_propose(&env, head)?;
                self.coordinators.insert(swap_id, coord);
                Ok(vec![(MessageType::SwapAccept, encode_swap(&accept)?)])
            }
            SwapKind::Accept => {
                self.route(&swap_id)?.recv_accept(&env, head)?;
                Ok(vec![])
            }
            SwapKind::FundingProof => {
                self.route(&swap_id)?.recv_funding_proof(&env)?;
                Ok(vec![])
            }
            SwapKind::PreimageReveal => {
                // The node extracts S from the carried BTC claim and drives the NIM claim itself.
                self.route(&swap_id)?; // just validate we know this swap
                Ok(vec![])
            }
            SwapKind::Abort => {
                self.coordinators.remove(&swap_id);
                Ok(vec![])
            }
        }
    }

    fn route(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Result<&mut SwapCoordinator, SessionError> {
        self.coordinators
            .get_mut(swap_id)
            .ok_or(SessionError::UnknownSwap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap::{SwapPhase, SwapTerms};
    use crate::swap_leg::sha256;

    fn identity(seed: u8) -> NodeIdentity {
        let mut pk = [seed; BTC_PUBKEY_LEN];
        pk[0] = 0x02;
        NodeIdentity {
            nim_address: [seed; NIM_ADDRESS_LEN],
            btc_address: b"tb1qnode".to_vec(),
            btc_pubkey: pk,
        }
    }

    fn ctx(swap_id: [u8; SWAP_ID_LEN], seed: u8) -> SwapContext {
        let mut pk = [seed; BTC_PUBKEY_LEN];
        pk[0] = 0x02;
        SwapContext {
            swap_id,
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: sha256(&[42u8; 32]),
            nim_address: [seed; NIM_ADDRESS_LEN],
            btc_address: b"tb1qnode".to_vec(),
            btc_pubkey: pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
        }
    }

    fn only(out: Vec<(MessageType, Vec<u8>)>) -> (MessageType, Vec<u8>) {
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap()
    }

    #[test]
    fn two_sessions_route_a_full_swap_to_settled() {
        let head = 0;
        let p = LadderParams::default();
        let secret = [42u8; 32];
        let swap_id = [0x7A; SWAP_ID_LEN];

        let mut alice = SwapSession::new(identity(0x11), p);
        let mut bob = SwapSession::new(identity(0x22), p);

        // Alice starts an initiator coordinator + floods the Propose.
        let (coord, propose) = SwapCoordinator::new_initiator(ctx(swap_id, 0x11), secret, p);
        alice.add_initiator(swap_id, coord);
        let propose_bytes = encode_swap(&propose).unwrap();

        // Bob's session routes the Propose → spins up a responder → returns the Accept.
        let (mt, accept_bytes) = only(
            bob.on_message(SwapKind::Propose, &propose_bytes, head)
                .unwrap(),
        );
        assert_eq!(mt, MessageType::SwapAccept);
        assert_eq!(bob.len(), 1);

        // Alice's session routes the Accept to her initiator coordinator (no outgoing).
        assert!(alice
            .on_message(SwapKind::Accept, &accept_bytes, head)
            .unwrap()
            .is_empty());

        // Alice funds NIM (node action) → FundingProof → Bob's session routes it.
        let nim_fp = encode_swap(
            &alice
                .coordinator(&swap_id)
                .unwrap()
                .fund(head, vec![0x11; 248], [0xC1; 32])
                .unwrap(),
        )
        .unwrap();
        assert!(bob
            .on_message(SwapKind::FundingProof, &nim_fp, head)
            .unwrap()
            .is_empty());

        // Bob funds BTC → FundingProof → Alice's session routes it (both BothFunded).
        let btc_fp = encode_swap(
            &bob.coordinator(&swap_id)
                .unwrap()
                .fund(head, vec![0x22; 120], [0xC2; 32])
                .unwrap(),
        )
        .unwrap();
        assert!(alice
            .on_message(SwapKind::FundingProof, &btc_fp, head)
            .unwrap()
            .is_empty());
        assert_eq!(
            alice.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::BothFunded
        );

        // Alice claims BTC (reveals S) → PreimageReveal → Bob's session routes it; Bob's node
        // extracts S and drives the NIM claim via the coordinator.
        let reveal = encode_swap(
            &alice
                .coordinator(&swap_id)
                .unwrap()
                .claim_and_reveal(secret.to_vec(), [0xC3; 32])
                .unwrap(),
        )
        .unwrap();
        assert!(bob
            .on_message(SwapKind::PreimageReveal, &reveal, head)
            .unwrap()
            .is_empty());
        bob.coordinator(&swap_id)
            .unwrap()
            .recv_reveal(
                &decode_swap(SwapKind::PreimageReveal, &reveal).unwrap(),
                secret,
            )
            .unwrap();

        // Both settle.
        alice.coordinator(&swap_id).unwrap().settle().unwrap();
        bob.coordinator(&swap_id).unwrap().settle().unwrap();
        assert_eq!(
            alice.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Settled
        );
        assert_eq!(
            bob.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Settled
        );
    }

    #[test]
    fn two_concurrent_proposes_route_to_separate_coordinators() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        for swap_id in [[0x01; SWAP_ID_LEN], [0x02; SWAP_ID_LEN]] {
            let (_c, propose) = SwapCoordinator::new_initiator(
                ctx(swap_id, 0x11),
                [42u8; 32],
                LadderParams::default(),
            );
            let (mt, _) = only(
                bob.on_message(SwapKind::Propose, &encode_swap(&propose).unwrap(), 0)
                    .unwrap(),
            );
            assert_eq!(mt, MessageType::SwapAccept);
        }
        assert_eq!(bob.len(), 2);
        assert!(bob.coordinator(&[0x01; SWAP_ID_LEN]).is_some());
        assert!(bob.coordinator(&[0x02; SWAP_ID_LEN]).is_some());
    }

    #[test]
    fn a_message_for_an_unknown_swap_is_rejected() {
        let mut s = SwapSession::new(identity(0x22), LadderParams::default());
        // An Accept with no matching initiator coordinator → UnknownSwap.
        let accept = crate::swap_messages::SwapAcceptance {
            swap_id: [0xEE; SWAP_ID_LEN],
            nim_address: [0; NIM_ADDRESS_LEN],
            btc_address: b"tb1q".to_vec(),
            btc_pubkey: [0x03; BTC_PUBKEY_LEN],
        }
        .to_envelope();
        assert_eq!(
            s.on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0),
            Err(SessionError::UnknownSwap)
        );
    }
}
