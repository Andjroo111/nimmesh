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
    decode_swap, encode_swap, SwapEnvelope, SwapKind, SwapWireError, BTC_PUBKEY_LEN,
    NIM_ADDRESS_LEN, SWAP_ID_LEN,
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

    /// A snapshot of every in-flight swap's `(swap_id, phase)` — the node mirrors this into shared
    /// state so the FFI side can observe a swap's progress without reaching into the worker thread.
    pub fn phases(&self) -> Vec<([u8; SWAP_ID_LEN], crate::swap::SwapPhase)> {
        self.coordinators
            .iter()
            .map(|(id, c)| (*id, c.phase()))
            .collect()
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
        // A malformed payload is dropped here (panic-free decode of untrusted mesh bytes) — no
        // coordinator is touched, so a hostile frame can neither create nor corrupt state.
        let env = decode_swap(kind, payload)?;
        let swap_id = env.swap_id;
        match kind {
            SwapKind::Propose => {
                // Replay / collision guard: a `Propose` for a `swap_id` we already track must NEVER
                // clobber the in-flight coordinator (whether one we initiated or an accepted
                // responder). Drop the duplicate with no reply — the live swap is untouched.
                if self.coordinators.contains_key(&swap_id) {
                    return Ok(Vec::new());
                }
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
                // Gate BEFORE inserting: a rejected `Propose` (unsafe ladder, mismatched fields)
                // leaves no half-built coordinator stranded in the map.
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
                // The counterparty bailed: free the slot — but ONLY if WE have no funds locked. A
                // counterparty abort must never drop our funded coordinator; that leg still has to
                // refund via the timeout path (a dropped coordinator would strand the refund). A
                // stray Abort for an unknown swap is a harmless no-op.
                if self
                    .coordinators
                    .get(&swap_id)
                    .is_some_and(|c| !c.phase().has_funds_locked())
                {
                    self.coordinators.remove(&swap_id);
                }
                Ok(vec![])
            }
        }
    }

    fn route(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Result<&mut SwapCoordinator, SessionError> {
        self.coordinators
            .get_mut(swap_id)
            .ok_or(SessionError::UnknownSwap)
    }

    /// Drop every coordinator that has reached a terminal phase (Settled / Aborted / Refunded),
    /// freeing its slot. Returns how many were reaped. The node calls this after it settles a swap;
    /// the session GC tick (G16) calls it to keep a node from being memory-exhausted by stale swaps.
    pub fn reap_terminal(&mut self) -> usize {
        let before = self.coordinators.len();
        self.coordinators.retain(|_, c| !c.phase().is_terminal());
        before - self.coordinators.len()
    }

    /// Forget a swap (e.g. the node aborted it locally), freeing its slot. Returns whether one was
    /// present.
    pub fn remove(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> bool {
        self.coordinators.remove(swap_id).is_some()
    }

    /// Locally cancel an **un-funded** swap: drop it and return a `SwapAbort` envelope to flood so
    /// the counterparty frees its slot too (symmetric teardown). Returns `None` — and changes
    /// nothing — if the swap is unknown or holds funds: a funded swap must **refund** via the
    /// timeout path, never abort (aborting would strand the counterparty's funded leg).
    pub fn cancel(&mut self, swap_id: &[u8; SWAP_ID_LEN]) -> Option<SwapEnvelope> {
        let c = self.coordinators.get(swap_id)?;
        if c.phase().has_funds_locked() {
            return None;
        }
        self.coordinators.remove(swap_id);
        Some(crate::swap_messages::abort(*swap_id, 0))
    }

    /// GC + safety-exit tick. First, any funds-locked swap whose own timeout has passed **refunds
    /// itself** — the always-available safety exit when a swap stalls (worst case: you get your own
    /// funds back). Then drop every coordinator that is terminal (Settled/Aborted/Refunded, incl.
    /// the just-refunded) or **stale** — a non-funded negotiation whose timelock has passed — so a
    /// long-lived node can't be memory-exhausted by half-opened or abandoned swaps. Returns the
    /// swap_ids dropped as **stale un-funded** negotiations — the ones for which the node should
    /// flood a teardown `SwapAbort` so the counterparty frees its slot too (a refund-reaped or
    /// already-terminal swap is dropped but NOT returned: it needs no teardown message). The node
    /// drives this off its worker maintenance tick.
    pub fn tick(&mut self, head: u64) -> Vec<[u8; SWAP_ID_LEN]> {
        // Safety exit: a funds-locked swap past its timeout refunds itself (no-op for non-funded or
        // not-yet-due swaps). In the sim the transition is the refund; production broadcasts the tx.
        for c in self.coordinators.values_mut() {
            let _ = c.refund_after_timeout(head);
        }
        // Stale = un-funded + non-terminal + past its timelock — these get a teardown Abort.
        let stale: Vec<[u8; SWAP_ID_LEN]> = self
            .coordinators
            .iter()
            .filter(|(_, c)| c.is_stale(head))
            .map(|(id, _)| *id)
            .collect();
        self.coordinators
            .retain(|_, c| !c.phase().is_terminal() && !c.is_stale(head));
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap::{SwapPhase, SwapTerms};
    use crate::swap_leg::sha256;
    use crate::swap_wire::SwapLegId;

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
        // Before its timeout a funds-locked swap is kept — the refund isn't due, and it's never
        // "stale" while it holds funds, so the GC tick leaves it alone to finish settling.
        assert!(alice.tick(5_000).is_empty());
        assert_eq!(alice.len(), 1);

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

        // Settled is terminal: reaping frees both slots so the node isn't left holding dead swaps.
        assert_eq!(alice.reap_terminal(), 1);
        assert_eq!(bob.reap_terminal(), 1);
        assert!(alice.is_empty() && bob.is_empty());
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

    // --- G15: hostile-mesh robustness -------------------------------------------------

    /// Build a valid `Propose` payload for `swap_id` (the initiator's, identical on every call so a
    /// resend is a true replay).
    fn propose_bytes(swap_id: [u8; SWAP_ID_LEN]) -> Vec<u8> {
        let (_c, propose) =
            SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
        encode_swap(&propose).unwrap()
    }

    #[test]
    fn a_replayed_propose_does_not_clobber_the_live_coordinator() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        let swap_id = [0x55; SWAP_ID_LEN];
        let propose = propose_bytes(swap_id);

        // Bob accepts, then observes the initiator's NIM funding → InitiatorFunded.
        only(bob.on_message(SwapKind::Propose, &propose, 0).unwrap());
        let nim_fp = encode_swap(&crate::swap_messages::tx_envelope(
            swap_id,
            SwapLegId::Nim,
            vec![0x11; 248],
            [0xC1; 32],
        ))
        .unwrap();
        bob.on_message(SwapKind::FundingProof, &nim_fp, 0).unwrap();
        assert_eq!(
            bob.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::InitiatorFunded
        );

        // A replayed identical Propose must be dropped (no reply) and must NOT reset the coordinator
        // back to Accepted — the in-flight swap is untouched.
        assert!(bob
            .on_message(SwapKind::Propose, &propose, 0)
            .unwrap()
            .is_empty());
        assert_eq!(bob.len(), 1);
        assert_eq!(
            bob.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::InitiatorFunded
        );
    }

    #[test]
    fn a_malformed_payload_creates_no_coordinator() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        // Garbage bytes for every kind → a structured error, never a panic, never a coordinator.
        for kind in [
            SwapKind::Propose,
            SwapKind::Accept,
            SwapKind::FundingProof,
            SwapKind::PreimageReveal,
            SwapKind::Abort,
        ] {
            let _ = bob.on_message(kind, &[0xFF; 7], 0); // Ok or Err — must not panic.
            assert!(bob.is_empty(), "a malformed {kind:?} half-created state");
        }
    }

    #[test]
    fn an_abort_frees_the_slot() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        let swap_id = [0x66; SWAP_ID_LEN];
        only(
            bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
                .unwrap(),
        );
        assert_eq!(bob.len(), 1);

        let abort = encode_swap(&crate::swap_messages::abort(swap_id, 0)).unwrap();
        assert!(bob
            .on_message(SwapKind::Abort, &abort, 0)
            .unwrap()
            .is_empty());
        assert!(bob.is_empty());
        // A stray Abort for an unknown swap is a harmless no-op.
        assert!(bob
            .on_message(SwapKind::Abort, &abort, 0)
            .unwrap()
            .is_empty());
    }

    // --- G16: session expiry / GC tick ------------------------------------------------

    #[test]
    fn tick_drops_a_stale_unfunded_negotiation_past_its_deadline() {
        // A swap that accepted but never funded is dead once the head passes its T_A timelock
        // (nim_timeout = 10_000 in the fixtures) — it can no longer be funded or claimed.
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        let swap_id = [0x77; SWAP_ID_LEN];
        only(
            bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
                .unwrap(),
        );
        assert_eq!(
            bob.coordinator(&swap_id).unwrap().phase(),
            SwapPhase::Accepted
        );

        // At/under the deadline it is kept; once the head passes T_A it is GC'd — and reported as a
        // stale un-funded drop (so the node floods a teardown Abort for it).
        assert!(bob.tick(10_000).is_empty());
        assert_eq!(bob.len(), 1);
        assert_eq!(bob.tick(10_001), vec![swap_id]);
        assert!(bob.is_empty());
    }

    #[test]
    fn tick_keeps_a_fresh_negotiation_before_its_deadline() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        let swap_id = [0x78; SWAP_ID_LEN];
        only(
            bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
                .unwrap(),
        );
        assert!(bob.tick(0).is_empty());
        assert_eq!(bob.len(), 1);
    }

    #[test]
    fn tick_refunds_then_reaps_a_funds_locked_swap_past_its_timeout() {
        // G18: a swap stalls with this node's funds locked. The tick's safety exit refunds the leg
        // once the head passes its timeout, then reaps it — "worst case is a refund", not a leak.
        let mut alice = SwapSession::new(identity(0x11), LadderParams::default());
        let swap_id = [0x99; SWAP_ID_LEN];
        let (coord, _propose) =
            SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
        alice.add_initiator(swap_id, coord);

        // Drive alice to a funds-locked phase: accept (from a counterparty), then fund the NIM leg.
        let accept = crate::swap_messages::SwapAcceptance {
            swap_id,
            nim_address: [0xB2; NIM_ADDRESS_LEN],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: [0x03; BTC_PUBKEY_LEN],
        }
        .to_envelope();
        alice
            .on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0)
            .unwrap();
        alice
            .coordinator(&swap_id)
            .unwrap()
            .fund(0, vec![0x11; 248], [0xC1; 32])
            .unwrap();
        assert!(alice
            .coordinator(&swap_id)
            .unwrap()
            .phase()
            .has_funds_locked());

        // The counterparty never funds. Before alice's T_A (10_000) the swap is kept; once the head
        // passes it, the tick refunds her leg and reaps it. A refund-reaped swap is NOT reported as
        // a stale abort (no teardown message — the counterparty handles its own refund).
        assert!(alice.tick(5_000).is_empty());
        assert_eq!(alice.len(), 1);
        assert!(alice.tick(10_001).is_empty());
        assert!(alice.is_empty());
    }

    // --- G19: abort emission + symmetric teardown ------------------------------------

    /// Drive a fresh session's coordinator to a funds-locked phase (initiator funds NIM). Returns
    /// the session + the swap_id.
    fn funds_locked_session() -> (SwapSession, [u8; SWAP_ID_LEN]) {
        let mut alice = SwapSession::new(identity(0x11), LadderParams::default());
        let swap_id = [0xAB; SWAP_ID_LEN];
        let (coord, _propose) =
            SwapCoordinator::new_initiator(ctx(swap_id, 0x11), [42u8; 32], LadderParams::default());
        alice.add_initiator(swap_id, coord);
        let accept = crate::swap_messages::SwapAcceptance {
            swap_id,
            nim_address: [0xB2; NIM_ADDRESS_LEN],
            btc_address: b"tb1qbob".to_vec(),
            btc_pubkey: [0x03; BTC_PUBKEY_LEN],
        }
        .to_envelope();
        alice
            .on_message(SwapKind::Accept, &encode_swap(&accept).unwrap(), 0)
            .unwrap();
        alice
            .coordinator(&swap_id)
            .unwrap()
            .fund(0, vec![0x11; 248], [0xC1; 32])
            .unwrap();
        (alice, swap_id)
    }

    #[test]
    fn cancel_emits_an_abort_and_frees_an_unfunded_swap() {
        let mut bob = SwapSession::new(identity(0x22), LadderParams::default());
        let swap_id = [0xCD; SWAP_ID_LEN];
        only(
            bob.on_message(SwapKind::Propose, &propose_bytes(swap_id), 0)
                .unwrap(),
        );
        let abort = bob
            .cancel(&swap_id)
            .expect("an un-funded swap can be cancelled");
        assert_eq!(abort.swap_id, swap_id);
        assert!(bob.is_empty());
        // Cancelling an unknown swap is a no-op (no abort).
        assert!(bob.cancel(&swap_id).is_none());
    }

    #[test]
    fn cancel_refuses_a_funds_locked_swap() {
        // A funded swap must REFUND, never abort — cancel returns None and keeps the coordinator.
        let (mut alice, swap_id) = funds_locked_session();
        assert!(alice.cancel(&swap_id).is_none());
        assert_eq!(alice.len(), 1);
    }

    #[test]
    fn an_inbound_abort_never_drops_a_funds_locked_coordinator() {
        // A counterparty abort must not strand our funded leg — we keep it (it refunds via timeout).
        let (mut alice, swap_id) = funds_locked_session();
        let abort = encode_swap(&crate::swap_messages::abort(swap_id, 0)).unwrap();
        assert!(alice
            .on_message(SwapKind::Abort, &abort, 0)
            .unwrap()
            .is_empty());
        assert_eq!(alice.len(), 1);
        assert!(alice
            .coordinator(&swap_id)
            .unwrap()
            .phase()
            .has_funds_locked());
    }
}
