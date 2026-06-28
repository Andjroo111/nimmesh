//! # swap_coordinator — the protocol brain a mesh node runs (one side of a swap)
//!
//! Wraps a [`crate::swap::Swap`] state machine + the swap context and turns the **message protocol**
//! into method calls: feed it the peer's envelope (or report a local funding/claim tx) and it
//! advances the swap and hands back the next outgoing envelope. Two coordinators (initiator +
//! responder) exchanging envelopes drive a full swap with no hand orchestration — exactly what a
//! [`crate::node::MeshNode`] would do on top of the flood/relay path. Pure: no keys, no bitcoin, no
//! tx-building (the node builds + broadcasts the txs and reports them back); the coordinator owns the
//! *coordination*. The on-chain tx bytes are opaque blobs to it.

use crate::swap::{LadderParams, Swap, SwapError, SwapPhase, SwapRole, SwapTerms};
use crate::swap_leg::sha256;
use crate::swap_messages::{tx_envelope, SwapAcceptance, SwapProposal};
use crate::swap_wire::{
    SwapEnvelope, SwapLegId, BTC_PUBKEY_LEN, HASH_LEN, NIM_ADDRESS_LEN, SWAP_ID_LEN,
};

/// A coordinator-level failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordError {
    /// The state machine rejected the implied transition.
    Swap(SwapError),
    /// A received envelope was malformed or for the wrong swap/leg.
    BadMessage {
        /// Why.
        reason: &'static str,
    },
    /// A revealed preimage did not hash to the agreed hashlock.
    BadPreimage,
}

impl From<SwapError> for CoordError {
    fn from(e: SwapError) -> Self {
        CoordError::Swap(e)
    }
}

/// The local identity + economics a node brings to a swap (everything not learned from the peer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapContext {
    /// The 16-byte per-swap correlator.
    pub swap_id: [u8; SWAP_ID_LEN],
    /// The agreed timelocks (`T_A` / `T_B`, Unix-ms).
    pub terms: SwapTerms,
    /// `H = SHA-256(secret)`.
    pub hashlock: [u8; HASH_LEN],
    /// This node's NIM address (20 raw bytes).
    pub nim_address: [u8; NIM_ADDRESS_LEN],
    /// This node's BTC payout address bytes.
    pub btc_address: Vec<u8>,
    /// This node's BTC pubkey (33 bytes).
    pub btc_pubkey: [u8; BTC_PUBKEY_LEN],
    /// NIM the initiator gives (luna) / responder mirrors.
    pub give_amount: u64,
    /// BTC the initiator wants (sat).
    pub take_amount: u64,
    /// The Albatross network id for the NIM leg.
    pub network_id: u8,
}

/// One side of a swap. Construct via [`new_initiator`](Self::new_initiator) /
/// [`new_responder`](Self::new_responder), then drive it with the `recv_*` / `fund` / `claim` /
/// `settle` methods as messages arrive and local txs are built.
pub struct SwapCoordinator {
    swap: Swap,
    ctx: SwapContext,
    secret: Option<[u8; HASH_LEN]>,
    peer_btc_pubkey: Option<[u8; BTC_PUBKEY_LEN]>,
    ladder: LadderParams,
}

impl SwapCoordinator {
    /// The **initiator**: returns the coordinator + the `Propose` envelope to flood.
    pub fn new_initiator(
        ctx: SwapContext,
        secret: [u8; HASH_LEN],
        ladder: LadderParams,
    ) -> (Self, SwapEnvelope) {
        let propose = SwapProposal {
            swap_id: ctx.swap_id,
            hashlock: ctx.hashlock,
            give_amount: ctx.give_amount,
            take_amount: ctx.take_amount,
            terms: ctx.terms,
            nim_address: ctx.nim_address,
            btc_address: ctx.btc_address.clone(),
            btc_pubkey: ctx.btc_pubkey,
            network_id: ctx.network_id,
        }
        .to_envelope();
        (
            SwapCoordinator {
                swap: Swap::new(SwapRole::Initiator, ctx.terms),
                ctx,
                secret: Some(secret),
                peer_btc_pubkey: None,
                ladder,
            },
            propose,
        )
    }

    /// The **responder** — waits for the initiator's `Propose`.
    pub fn new_responder(ctx: SwapContext, ladder: LadderParams) -> Self {
        SwapCoordinator {
            swap: Swap::new(SwapRole::Responder, ctx.terms),
            ctx,
            secret: None,
            peer_btc_pubkey: None,
            ladder,
        }
    }

    /// This node's lifecycle phase.
    pub fn phase(&self) -> SwapPhase {
        self.swap.phase
    }

    /// The peer's BTC pubkey, once it has been exchanged (both halves build the same HTLC).
    pub fn peer_btc_pubkey(&self) -> Option<[u8; BTC_PUBKEY_LEN]> {
        self.peer_btc_pubkey
    }

    /// (Responder) handle the initiator's `Propose`: learn its claimant pubkey, accept, return the
    /// `Accept` envelope to flood back.
    pub fn recv_propose(
        &mut self,
        env: &SwapEnvelope,
        head: u64,
    ) -> Result<SwapEnvelope, CoordError> {
        let p = SwapProposal::from_envelope(env).ok_or(CoordError::BadMessage {
            reason: "incomplete Propose",
        })?;
        if p.swap_id != self.ctx.swap_id || p.hashlock != self.ctx.hashlock {
            return Err(CoordError::BadMessage {
                reason: "Propose is for a different swap",
            });
        }
        self.peer_btc_pubkey = Some(p.btc_pubkey);
        self.swap.accept(head, &self.ladder)?;
        Ok(SwapAcceptance {
            swap_id: self.ctx.swap_id,
            nim_address: self.ctx.nim_address,
            btc_address: self.ctx.btc_address.clone(),
            btc_pubkey: self.ctx.btc_pubkey,
        }
        .to_envelope())
    }

    /// (Initiator) handle the responder's `Accept`: learn its funder pubkey, accept.
    pub fn recv_accept(&mut self, env: &SwapEnvelope, head: u64) -> Result<(), CoordError> {
        let a = SwapAcceptance::from_envelope(env).ok_or(CoordError::BadMessage {
            reason: "incomplete Accept",
        })?;
        if a.swap_id != self.ctx.swap_id {
            return Err(CoordError::BadMessage {
                reason: "Accept is for a different swap",
            });
        }
        self.peer_btc_pubkey = Some(a.btc_pubkey);
        self.swap.accept(head, &self.ladder)?;
        Ok(())
    }

    /// Fund this node's own leg (the node already built + broadcast the signed `tx_wire`) and return
    /// the `FundingProof` envelope to flood. The responder also records BothFunded (it already saw
    /// the initiator's funding).
    pub fn fund(
        &mut self,
        head: u64,
        tx_wire: Vec<u8>,
        tx_id: [u8; HASH_LEN],
    ) -> Result<SwapEnvelope, CoordError> {
        self.swap.fund(head, &self.ladder)?;
        let own_leg = self.swap.own_leg();
        if self.swap.role == SwapRole::Responder {
            self.swap.observe_counterparty_funded()?;
        }
        Ok(tx_envelope(self.ctx.swap_id, own_leg, tx_wire, tx_id))
    }

    /// Handle the peer's `FundingProof`: the initiator observes the counterparty leg funded
    /// (BothFunded); the responder observes the initiator's funding (it can now fund its own).
    pub fn recv_funding_proof(&mut self, env: &SwapEnvelope) -> Result<(), CoordError> {
        let leg = env.leg.ok_or(CoordError::BadMessage {
            reason: "FundingProof has no leg",
        })?;
        match self.swap.role {
            SwapRole::Initiator => {
                if leg != SwapLegId::Counterparty {
                    return Err(CoordError::BadMessage {
                        reason: "expected the counterparty leg",
                    });
                }
                self.swap.observe_counterparty_funded()?;
            }
            SwapRole::Responder => {
                if leg != SwapLegId::Nim {
                    return Err(CoordError::BadMessage {
                        reason: "expected the NIM leg",
                    });
                }
                self.swap.observe_initiator_funded()?;
            }
        }
        Ok(())
    }

    /// (Initiator) claim the counterparty (BTC) leg, revealing `S` — the node built the claim
    /// `tx_wire` (which carries `S`). Returns the `PreimageReveal` envelope to flood.
    pub fn claim_and_reveal(
        &mut self,
        tx_wire: Vec<u8>,
        tx_id: [u8; HASH_LEN],
    ) -> Result<SwapEnvelope, CoordError> {
        self.swap.reveal_and_claim()?;
        Ok(tx_envelope(
            self.ctx.swap_id,
            SwapLegId::Counterparty,
            tx_wire,
            tx_id,
        ))
    }

    /// (Responder) handle the `PreimageReveal`: `secret` was extracted from the BTC claim (by the
    /// node, via `btc::extract_preimage`). Verify it opens the hashlock, then advance — the node now
    /// claims the NIM leg with it. Returns the verified secret.
    pub fn recv_reveal(
        &mut self,
        _env: &SwapEnvelope,
        secret: [u8; HASH_LEN],
    ) -> Result<[u8; HASH_LEN], CoordError> {
        if sha256(&secret) != self.ctx.hashlock {
            return Err(CoordError::BadPreimage);
        }
        self.swap.observe_secret()?;
        self.secret = Some(secret);
        Ok(secret)
    }

    /// Record that this node's claim settled on-chain → terminal success.
    pub fn settle(&mut self) -> Result<(), CoordError> {
        self.swap.observe_settled()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap_wire::{decode_swap, encode_swap, SwapKind};

    fn ctx(seed: u8, nim: u8) -> SwapContext {
        let mut pk = [seed; BTC_PUBKEY_LEN];
        pk[0] = 0x02;
        SwapContext {
            swap_id: [0x7A; SWAP_ID_LEN],
            terms: SwapTerms {
                nim_timeout: 10_000,
                counterparty_timeout: 5_000,
            },
            hashlock: sha256(&[42u8; 32]),
            nim_address: [nim; NIM_ADDRESS_LEN],
            btc_address: b"tb1qnode".to_vec(),
            btc_pubkey: pk,
            give_amount: 100_000,
            take_amount: 50_000,
            network_id: 5,
        }
    }

    /// Round-trip an envelope through the wire codec (so the test exercises real bytes, not structs).
    fn wire(kind: SwapKind, env: &SwapEnvelope) -> SwapEnvelope {
        decode_swap(kind, &encode_swap(env).unwrap()).unwrap()
    }

    #[test]
    fn two_coordinators_drive_a_full_swap_by_exchanging_envelopes() {
        let head = 0;
        let p = LadderParams::default();
        let secret = [42u8; 32];

        let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), secret, p);
        let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);

        // Propose → Accept (pubkeys exchanged over the wire).
        let accept = bob
            .recv_propose(&wire(SwapKind::Propose, &propose), head)
            .unwrap();
        alice
            .recv_accept(&wire(SwapKind::Accept, &accept), head)
            .unwrap();
        assert_eq!(alice.peer_btc_pubkey(), Some(ctx(0x22, 0).btc_pubkey));
        assert_eq!(bob.peer_btc_pubkey(), Some(ctx(0x11, 0).btc_pubkey));

        // Alice funds NIM → FundingProof → Bob observes.
        let nim_fp = alice.fund(head, vec![0x11; 248], [0xC1; 32]).unwrap();
        bob.recv_funding_proof(&wire(SwapKind::FundingProof, &nim_fp))
            .unwrap();

        // Bob funds BTC → FundingProof → Alice observes (BothFunded on both sides).
        let btc_fp = bob.fund(head, vec![0x22; 120], [0xC2; 32]).unwrap();
        alice
            .recv_funding_proof(&wire(SwapKind::FundingProof, &btc_fp))
            .unwrap();
        assert_eq!(alice.phase(), SwapPhase::BothFunded);
        assert_eq!(bob.phase(), SwapPhase::BothFunded);

        // Alice claims BTC (reveals S) → PreimageReveal → Bob reads S, claims NIM.
        let reveal = alice.claim_and_reveal(secret.to_vec(), [0xC3; 32]).unwrap();
        let learned = bob
            .recv_reveal(&wire(SwapKind::PreimageReveal, &reveal), secret)
            .unwrap();
        assert_eq!(learned, secret);

        // Both settle.
        alice.settle().unwrap();
        bob.settle().unwrap();
        assert_eq!(alice.phase(), SwapPhase::Settled);
        assert_eq!(bob.phase(), SwapPhase::Settled);
    }

    #[test]
    fn a_reveal_with_the_wrong_secret_is_rejected() {
        let head = 0;
        let p = LadderParams::default();
        let (mut alice, propose) = SwapCoordinator::new_initiator(ctx(0x11, 0xA1), [42u8; 32], p);
        let mut bob = SwapCoordinator::new_responder(ctx(0x22, 0xB2), p);
        let accept = bob.recv_propose(&propose, head).unwrap();
        alice.recv_accept(&accept, head).unwrap();
        let nim_fp = alice.fund(head, vec![0x11; 248], [0xC1; 32]).unwrap();
        bob.recv_funding_proof(&nim_fp).unwrap();
        let btc_fp = bob.fund(head, vec![0x22; 120], [0xC2; 32]).unwrap();
        alice.recv_funding_proof(&btc_fp).unwrap();
        let reveal = alice.claim_and_reveal(vec![0u8; 32], [0xC3; 32]).unwrap();
        // A secret that doesn't open the hashlock is refused — bob does not advance.
        assert_eq!(
            bob.recv_reveal(&reveal, [0x99u8; 32]),
            Err(CoordError::BadPreimage)
        );
        assert_eq!(bob.phase(), SwapPhase::BothFunded);
    }
}
