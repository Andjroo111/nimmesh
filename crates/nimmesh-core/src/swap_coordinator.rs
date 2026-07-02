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
use crate::swap_funding_verify::{
    require_funded, FundingRejected, FundingVerifier, HtlcExpectation,
};
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
    /// S2 / #73: a received `Propose` was not authenticated — it carried no signature, or the signed
    /// terms did not verify under a NIM key that hashes to the Propose's own `nim_address`. A relay
    /// that injected a proposal or tampered with the proposed terms in transit lands here, so the
    /// responder never spins up a coordinator on unauthenticated terms.
    UnauthenticProposal,
    /// The counterparty's HTLC could not be verified on-chain, so we refuse to fund our own leg (or
    /// reveal `S`). Carries the specific reason (not yet on-chain, too shallow, under-funded, timeout
    /// too short, or a hashlock/recipient mismatch) — a lie or a not-yet, either way we do not proceed.
    FundingUnverified(FundingRejected),
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

/// G31: a serializable capture of a coordinator's essential state for crash recovery. The node
/// persists it and rebuilds the coordinator on restart so a funds-locked swap can still refund.
/// **Holds the initiator secret** — store it as securely as a key; deliberately NO `Debug` so it is
/// never logged. The pending retransmit buffer is not part of it (a restarted node re-derives that).
#[derive(Clone, PartialEq, Eq)]
pub struct CoordinatorSnapshot {
    /// This node's role.
    pub role: SwapRole,
    /// The lifecycle phase at snapshot time.
    pub phase: SwapPhase,
    /// The full swap context (terms, hashlock, addresses, amounts, network).
    pub ctx: SwapContext,
    /// The initiator's secret `S` (`None` for a responder). Sensitive.
    pub secret: Option<[u8; HASH_LEN]>,
    /// The peer's BTC pubkey, once it has been learned.
    pub peer_btc_pubkey: Option<[u8; BTC_PUBKEY_LEN]>,
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

    /// This node's role in the swap (initiator funds NIM + claims BTC; responder mirrors).
    pub fn role(&self) -> SwapRole {
        self.swap.role
    }

    /// (Initiator) This node's swap secret `S`, for its **own** signer to embed in the claim tx it
    /// builds (the node and its signer are one trust domain; `S` never crosses the FFI boundary, and
    /// it reaches the counterparty only inside the `PreimageReveal` the signed claim produces). The
    /// responder has no secret, so this is `None` for it.
    pub(crate) fn secret(&self) -> Option<[u8; HASH_LEN]> {
        self.secret
    }

    /// G31: capture this coordinator's state for crash recovery (the node persists the snapshot and
    /// rebuilds the coordinator on restart so a funds-locked swap can still refund).
    pub fn to_snapshot(&self) -> CoordinatorSnapshot {
        CoordinatorSnapshot {
            role: self.swap.role,
            phase: self.swap.phase,
            ctx: self.ctx.clone(),
            secret: self.secret,
            peer_btc_pubkey: self.peer_btc_pubkey,
        }
    }

    /// G31: rebuild a coordinator from a snapshot + the node's ladder, at the captured phase, so its
    /// refund/claim re-arms exactly where the crash left it.
    pub fn from_snapshot(snapshot: CoordinatorSnapshot, ladder: LadderParams) -> Self {
        SwapCoordinator {
            swap: Swap {
                role: snapshot.role,
                terms: snapshot.ctx.terms,
                phase: snapshot.phase,
            },
            ctx: snapshot.ctx,
            secret: snapshot.secret,
            peer_btc_pubkey: snapshot.peer_btc_pubkey,
            ladder,
        }
    }

    /// Refund this node's own funded leg once its timeout has elapsed (`head > own timeout`) — the
    /// always-available safety exit when a swap stalls: the worst case is getting your own funds
    /// back. A no-op error (which the GC tick ignores) if the swap isn't funded or the timeout
    /// hasn't passed yet. Production builds + broadcasts the refund tx off this same trigger; in the
    /// sim the state transition *is* the refund.
    pub fn refund_after_timeout(&mut self, head: u64) -> Result<(), CoordError> {
        self.swap.refund_after_timeout(head)?;
        Ok(())
    }

    /// Whether this swap is **stale** at `head` and safe to forget: it is neither terminal nor holding
    /// any of this node's funds, and `head` has passed the point where the swap could still be **safely
    /// funded** — so the negotiation can never complete. A swap with funds locked is NEVER stale; its
    /// refund path must stay tracked until it reaches a terminal phase. No wall clock needed — the
    /// swap's own timelocks are the deadline.
    ///
    /// The reap deadline is `T_B − min_claim_window_blocks` (#75 / G4), **not** the far-later `T_A`: an
    /// un-funded swap is dead the instant its own [`fund`](Self::fund) would refuse `WindowTooShort`
    /// (the counterparty leg's claim window has closed), so a `Propose` flood of never-funded swaps
    /// frees its concurrency slots promptly instead of squatting until `T_A` (S5 slot-jam). Using `T_B`
    /// (the shorter leg) with the same claim-window margin funding uses keeps this consistent with the
    /// fund-time gate.
    pub fn is_stale(&self, head: u64) -> bool {
        let phase = self.swap.phase;
        if phase.is_terminal() || phase.has_funds_locked() {
            return false;
        }
        let last_fundable = self
            .ctx
            .terms
            .counterparty_timeout
            .saturating_sub(self.ladder.min_claim_window_blocks);
        head > last_fundable
    }

    /// (Responder) handle the initiator's `Propose`: learn its claimant pubkey, accept, return the
    /// `Accept` envelope to flood back.
    pub fn recv_propose(
        &mut self,
        env: &SwapEnvelope,
        head: u64,
    ) -> Result<SwapEnvelope, CoordError> {
        // S2 / #73: authenticate the Propose BEFORE touching any state. Its wire-carried signature must
        // verify under a NIM key that hashes to the Propose's own `nim_address`, so a relay can neither
        // inject a proposal nor tamper with the proposed terms in transit — an unsigned, forged, or
        // tampered Propose is dropped here and never spins up a responder coordinator.
        if !SwapProposal::verify_envelope(env) {
            return Err(CoordError::UnauthenticProposal);
        }
        let p = SwapProposal::from_envelope(env).ok_or(CoordError::BadMessage {
            reason: "incomplete Propose",
        })?;
        if p.swap_id != self.ctx.swap_id || p.hashlock != self.ctx.hashlock {
            return Err(CoordError::BadMessage {
                reason: "Propose is for a different swap",
            });
        }
        // Gate FIRST: a rejected accept (e.g. unsafe ladder) must leave no learned state behind.
        self.swap.accept(head, &self.ladder)?;
        self.peer_btc_pubkey = Some(p.btc_pubkey);
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
        self.swap.accept(head, &self.ladder)?; // gate first; on failure, learn nothing
        self.peer_btc_pubkey = Some(a.btc_pubkey);
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
    ///
    /// **Security (S1 / #72):** advancing off the *message* alone is unsafe with real funds — the
    /// sanctioned path is [`verify_and_observe_funding`](Self::verify_and_observe_funding), which only
    /// advances after confirming the counterparty HTLC on-chain. Slice 2 routes the node/session/engine
    /// through the verified path exclusively; this message handler stays for the sim/test transport.
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

    /// The on-chain HTLC we must verify before proceeding: the *counterparty's* leg. A responder
    /// verifies the initiator's NIM leg before funding its own BTC; an initiator verifies the
    /// responder's BTC leg before revealing `S`. `recipient` is *our* claim key on that leg — if the
    /// on-chain HTLC pays anyone else it is not ours to take. Amount/timeout floors are the agreed
    /// terms for that leg (more amount / later timeout only helps us; less is a lie).
    pub fn counterparty_expectation(&self) -> HtlcExpectation {
        match self.swap.role {
            // Responder verifies the initiator's NIM funding (claimable by us with `S`).
            SwapRole::Responder => HtlcExpectation {
                leg: SwapLegId::Nim,
                hashlock: self.ctx.hashlock,
                min_amount: self.ctx.give_amount,
                min_timeout: self.ctx.terms.nim_timeout,
                recipient: self.ctx.nim_address.to_vec(),
            },
            // Initiator verifies the responder's counterparty (BTC) funding (claimable by us with `S`).
            SwapRole::Initiator => HtlcExpectation {
                leg: SwapLegId::Counterparty,
                hashlock: self.ctx.hashlock,
                min_amount: self.ctx.take_amount,
                min_timeout: self.ctx.terms.counterparty_timeout,
                recipient: self.ctx.btc_pubkey.to_vec(),
            },
        }
    }

    /// Gate the funded-observed transition on an ON-CHAIN check of the counterparty's HTLC (S1 / #72).
    /// Observes the chain via `verifier`, applies [`require_funded`] against the agreed terms at
    /// `min_confirmations` depth, and ONLY on success advances the state machine — the responder to
    /// `InitiatorFunded` (so it may now fund), the initiator to `BothFunded` (so it may now reveal `S`).
    /// On any refusal it returns [`CoordError::FundingUnverified`] and does not move: the caller retries
    /// next tick, or refunds once the timelock passes. This is the anti-theft invariant: no funded
    /// transition is reachable without a matching, deep-enough, correctly-parameterised on-chain HTLC.
    pub fn verify_and_observe_funding(
        &mut self,
        verifier: &dyn FundingVerifier,
        min_confirmations: u32,
    ) -> Result<(), CoordError> {
        let expect = self.counterparty_expectation();
        let obs = verifier.observe(&expect);
        require_funded(&obs, &expect, min_confirmations).map_err(CoordError::FundingUnverified)?;
        match self.swap.role {
            SwapRole::Responder => self.swap.observe_initiator_funded()?,
            SwapRole::Initiator => self.swap.observe_counterparty_funded()?,
        }
        Ok(())
    }

    /// (Initiator) claim the counterparty (BTC) leg, revealing `S` — the node built the claim
    /// `tx_wire` (which carries `S`). Gated on the reveal deadline at `head` (G4 / #75): if `T_B` is
    /// within the claim window it refuses (`CoordError::Swap(RevealTooLate)`) and does NOT reveal, so
    /// the node keeps `S` secret and refunds once `T_B` passes. Returns the `PreimageReveal` envelope
    /// to flood on success.
    pub fn claim_and_reveal(
        &mut self,
        head: u64,
        tx_wire: Vec<u8>,
        tx_id: [u8; HASH_LEN],
    ) -> Result<SwapEnvelope, CoordError> {
        self.swap.reveal_and_claim(head, &self.ladder)?;
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
